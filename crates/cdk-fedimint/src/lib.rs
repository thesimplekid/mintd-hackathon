//! CDK lightning backend for CLN

use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use cdk::cdk_lightning::{self, Amount, BalanceResponse, MintLightning, PayInvoiceResponse};
use cdk::types::InvoiceStatus;
use cdk::Bolt11Invoice;
use error::Error;
use futures::stream::StreamExt;
use futures::Stream;
use hex;
use multimint::fedimint_client::ClientHandleArc;
use multimint::fedimint_core::api::InviteCode;
use multimint::fedimint_core::core::OperationId;
use multimint::fedimint_core::Amount as FedimintAmount;
use multimint::fedimint_ln_client::{
    InternalPayState, LightningClientModule, LnPayState, LnReceiveState, OutgoingLightningPayment,
    PayType,
};
use multimint::fedimint_ln_common::lightning_invoice::{
    Bolt11Invoice as FedimintBolt11Invoice, Bolt11InvoiceDescription, Description,
};
use multimint::MultiMint;
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, info};

pub mod error;

#[derive(Clone)]
pub struct Fedimint {
    pub client: ClientHandleArc,
    sender: tokio::sync::mpsc::Sender<Bolt11Invoice>,
    receiver: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<Bolt11Invoice>>>>,
}

impl Fedimint {
    pub async fn new(work_dir: PathBuf, invite_code: &str) -> Result<Self, Error> {
        let invite_code = InviteCode::from_str(invite_code)?;
        let mut multi_mint = MultiMint::new(work_dir).await?;
        let federation_id = multi_mint.register_new(invite_code, None).await?;
        multi_mint.update_gateway_caches().await?;

        let client = multi_mint
            .get(&federation_id)
            .await
            .ok_or(Error::WrongFedimintResponse)?;

        let (sender, receiver) = tokio::sync::mpsc::channel(8);

        Ok(Self {
            client,
            sender,
            receiver: Arc::new(Mutex::new(Some(receiver))),
        })
    }
}

#[async_trait]
impl MintLightning for Fedimint {
    type Err = cdk_lightning::Error;

    async fn wait_invoice(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Option<Bolt11Invoice>> + Send>>, Self::Err> {
        let receiver = self.receiver.lock().await.take().ok_or(Error::NoReceiver)?;
        let receiver_stream = ReceiverStream::new(receiver);
        Ok(Box::pin(receiver_stream.map(|invoice| Some(invoice))))
    }

    async fn pay_invoice(
        &self,
        bolt11: Bolt11Invoice,
        _partial_msat: Option<Amount>,
        _max_fee: Option<Amount>,
    ) -> Result<PayInvoiceResponse, Self::Err> {
        let lighting_module = self.client.get_first_module::<LightningClientModule>();
        let gateway_announcement = lighting_module.list_gateways().await;
        let first_gateway = gateway_announcement.first().ok_or(Error::NoGateways)?;

        let gateway = lighting_module
            .select_gateway(&first_gateway.info.gateway_id)
            .await
            .ok_or(Error::NoGateways)?;

        let OutgoingLightningPayment {
            payment_type,
            contract_id,
            fee,
        } = lighting_module
            .pay_bolt11_invoice(
                Some(gateway),
                FedimintBolt11Invoice::from_str(&bolt11.to_string())
                    .map_err(|_| Error::Description)?,
                (),
            )
            .await
            .map_err(|_| Error::Description)?;

        let operation_id = payment_type.operation_id();
        info!("Gateway fee: {fee}, payment operation id: {operation_id}");

        let ln_pay_response = self
            .wait_for_ln_payment(payment_type, contract_id.to_string(), false)
            .await?
            .ok_or_else(|| {
                error!("Payment failed");
                Error::PaymentFailed
            })?;

        let invoice_amount = bolt11
            .amount_milli_satoshis()
            .ok_or(Error::AmountRequired)?;

        let total_spent = Amount::from_msat(invoice_amount + ln_pay_response.fee.to_msat());

        Ok(PayInvoiceResponse {
            payment_hash: ln_pay_response.operation_id.to_string(),
            payment_preimage: Some(ln_pay_response.preimage),
            status: InvoiceStatus::Paid,
            total_spent,
        })
    }

    async fn get_balance(&self) -> Result<BalanceResponse, Self::Err> {
        todo!()
    }

    async fn create_invoice(
        &self,
        amount: Amount,
        description: String,
        unix_expiry: u64,
    ) -> Result<Bolt11Invoice, Self::Err> {
        let lighting_module = self.client.get_first_module::<LightningClientModule>();
        let gateway_announcement = lighting_module.list_gateways().await;
        let first_gateway = gateway_announcement.first().ok_or(Error::NoGateways)?;

        let gateway = lighting_module
            .select_gateway(&first_gateway.info.gateway_id)
            .await
            .ok_or(Error::NoGateways)?;

        let (operation_id, invoice, _something) = lighting_module
            .create_bolt11_invoice(
                FedimintAmount::from_msats(amount.to_msat()),
                Bolt11InvoiceDescription::Direct(
                    &Description::new(description.to_string()).map_err(|_| Error::Description)?,
                ),
                Some(unix_expiry),
                (),
                Some(gateway),
            )
            .await?;

        let client = self.client.clone();
        let sender = self.sender.clone();
        let invoice = cdk::Bolt11Invoice::from_str(&invoice.to_string()).unwrap();
        let invoice_clone = invoice.clone();
        tokio::spawn(async move {
            let lighting_module = client.get_first_module::<LightningClientModule>();

            let updates = lighting_module
                .subscribe_ln_receive(operation_id)
                .await
                .unwrap();

            let mut stream = updates.into_stream();

            while let Some(update) = stream.next().await {
                match update {
                    LnReceiveState::Claimed => {
                        sender.send(invoice_clone.clone()).await.unwrap();
                    }
                    _ => (),
                }
            }
        });

        Ok(invoice)
    }
}

impl Fedimint {
    pub async fn wait_for_ln_payment(
        &self,
        payment_type: PayType,
        contract_id: String,
        return_on_funding: bool,
    ) -> Result<Option<LnPayResponse>, Error> {
        let lightning_module = self.client.get_first_module::<LightningClientModule>();
        match payment_type {
            PayType::Internal(operation_id) => {
                let mut updates = lightning_module
                    .subscribe_internal_pay(operation_id)
                    .await?
                    .into_stream();

                while let Some(update) = updates.next().await {
                    match update {
                        InternalPayState::Preimage(preimage) => {
                            return Ok(Some(LnPayResponse {
                                operation_id,
                                payment_type,
                                contract_id,
                                fee: Amount::ZERO,
                                preimage: hex::encode(preimage.0),
                            }));
                        }
                        InternalPayState::RefundSuccess { out_points, error } => {
                            let e = format!(
                            "Internal payment failed. A refund was issued to {:?} Error: {error}",
                            out_points
                        );
                            return Err(Error::Custom(format!("{}", e)));
                        }
                        InternalPayState::UnexpectedError(e) => {
                            return Err(Error::Custom(format!("{}", e)));
                        }
                        InternalPayState::Funding if return_on_funding => return Ok(None),
                        InternalPayState::Funding => {}
                        InternalPayState::RefundError {
                            error_message,
                            error: _,
                        } => {
                            return Err(Error::Custom(format!("{}", error_message)));
                        }
                        InternalPayState::FundingFailed { error } => {
                            return Err(Error::Custom(format!("{}", error)));
                        }
                    }
                    info!("Update: {update:?}");
                }
            }
            PayType::Lightning(operation_id) => {
                let mut updates = lightning_module
                    .subscribe_ln_pay(operation_id)
                    .await?
                    .into_stream();

                while let Some(update) = updates.next().await {
                    let update_clone = update.clone();
                    match update_clone {
                        LnPayState::Success { preimage } => {
                            return Ok(Some(LnPayResponse {
                                operation_id,
                                payment_type,
                                contract_id,
                                fee: Amount::ZERO,
                                preimage,
                            }));
                        }
                        LnPayState::Refunded { gateway_error } => {
                            info!("{gateway_error}");
                            return Err(Error::Custom("{e}".to_string()));
                        }
                        LnPayState::Canceled => {
                            return Err(Error::Custom("{e}".to_string()));
                        }
                        LnPayState::Created
                        | LnPayState::AwaitingChange
                        | LnPayState::WaitingForRefund { .. } => {}
                        LnPayState::Funded if return_on_funding => return Ok(None),
                        LnPayState::Funded => {}
                        LnPayState::UnexpectedError { error_message } => {
                            return Err(Error::Custom(format!("{}", error_message)));
                        }
                    }
                    info!("Update: {update:?}");
                }
            }
        };
        return Err(Error::Custom("Lighting Payment Failed".to_string()));
    }
}

#[derive(Debug)]
pub struct LnPayResponse {
    pub operation_id: OperationId,
    pub payment_type: PayType,
    pub contract_id: String,
    pub fee: Amount,
    pub preimage: String,
}
