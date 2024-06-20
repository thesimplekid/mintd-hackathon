//! CDK lightning backend for CLN

use std::f32::consts::E;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cdk::cdk_lightning::{
    self, Amount, BalanceResponse, InvoiceInfo, MintLightning, PayInvoiceResponse,
};
use cdk::types::InvoiceStatus;
use cdk::{Bolt11Invoice, Sha256};
use error::Error;
use futures::stream::StreamExt;
use futures::Stream;
use multimint::fedimint_client::ClientHandleArc;
use multimint::fedimint_core::api::InviteCode;
use multimint::fedimint_core::Amount as FedimintAmount;
use multimint::fedimint_ln_client::{
    LightningClientModule, LnReceiveState, OutgoingLightningPayment,
};
use multimint::fedimint_ln_common::lightning_invoice::{Bolt11InvoiceDescription, Description};
use multimint::MultiMint;
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;

pub mod error;

#[derive(Clone)]
pub struct Fedimint {
    client: ClientHandleArc,
    sender: tokio::sync::mpsc::Sender<Bolt11Invoice>,
    receiver: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<Bolt11Invoice>>>>,
}

impl Fedimint {
    pub async fn new(work_dir: PathBuf, invite_code: InviteCode) -> Result<Self, Error> {
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

    async fn get_invoice(
        &self,
        amount: Amount,
        hash: &str,
        description: &str,
        //TODO: Add expiry
    ) -> Result<InvoiceInfo, Self::Err> {
        todo!()
    }

    async fn wait_invoice(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Option<Bolt11Invoice>> + Send>>, Self::Err> {
        let receiver = self.receiver.lock().await.take().ok_or(Error::NoReceiver)?;
        let receiver_stream = ReceiverStream::new(receiver);
        Ok(Box::pin(receiver_stream.map(|invoice| Some(invoice))))
    }

    async fn check_invoice_status(
        &self,
        payment_hash: &Sha256,
    ) -> Result<InvoiceStatus, Self::Err> {
        todo!()
    }

    async fn pay_invoice(
        &self,
        bolt11: Bolt11Invoice,
        partial_msat: Option<Amount>,
        max_fee: Option<Amount>,
    ) -> Result<PayInvoiceResponse, Self::Err> {
        let lighting_module = self.client.get_first_module::<LightningClientModule>();
        let gateway_announcment = lighting_module.list_gateways().await;
        let first_gateway = gateway_announcment.first().ok_or(Error::NoGateways)?;

        let gateway = lighting_module
            .select_gateway(&first_gateway.info.gateway_id)
            .await
            .ok_or(Error::NoGateways)?;

        let OutgoingLightningPayment {
            payment_type,
            contract_id,
            fee,
        } = lighting_module
            .pay_bolt11_invoice(Some(gateway), bolt11, ())
            .await
            .map_err(|_| Error::Description)?;
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
        let gateway_announcment = lighting_module.list_gateways().await;
        let first_gateway = gateway_announcment.first().ok_or(Error::NoGateways)?;

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
                        sender.send(invoice_clone.clone());
                    }
                    _ => (),
                }
            }
        });

        Ok(invoice)
    }
}
