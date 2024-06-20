use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::extract::{Json, Path, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_ORIGIN, AUTHORIZATION, CONTENT_TYPE,
};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use cdk::cdk_lightning::{self, Amount as CDKLightningAmount, MintLightning};
use cdk::error::{Error, ErrorResponse};
use cdk::mint::Mint;
use cdk::nuts::{
    CheckStateRequest, CheckStateResponse, CurrencyUnit, Id, KeysResponse, KeysetResponse,
    MeltBolt11Request, MeltBolt11Response, MeltQuoteBolt11Request, MeltQuoteBolt11Response,
    MintBolt11Request, MintBolt11Response, MintInfo, MintQuoteBolt11Request,
    MintQuoteBolt11Response, RestoreRequest, RestoreResponse, SwapRequest, SwapResponse,
};
use cdk::types::{
    MeltFedimintRequest, MeltFedimintResponse, MintFedimintRequest, MintFedimintResponse, MintQuote,
};
use cdk::util::unix_time;
use cdk::{Amount, Bolt11Invoice};
use futures::StreamExt;
use multimint::fedimint_client::ClientHandleArc;
use multimint::fedimint_core::Amount as FedimintAmount;
use multimint::fedimint_mint_client::{MintClientModule, ReissueExternalNotesState};
use tower_http::cors::CorsLayer;
use tracing::info;

pub async fn start_server(
    mint_url: &str,
    listen_addr: &str,
    listen_port: u16,
    mint: Mint,
    ln: Arc<dyn MintLightning<Err = cdk_lightning::Error> + Send + Sync>,
    fedimint_client: Option<ClientHandleArc>,
) -> Result<()> {
    let mint_clone = Arc::new(mint.clone());
    let ln_clone = ln.clone();
    tokio::spawn(async move {
        loop {
            let mut stream = ln_clone.wait_invoice().await.unwrap();

            while let Some(invoice) = stream.next().await {
                if let Err(err) = handle_paid_invoice(mint_clone.clone(), invoice).await {
                    tracing::warn!("{:?}", err);
                }
            }
        }
    });

    let state = MintState {
        ln,
        mint,
        mint_url: mint_url.to_string(),
        fedimint_client: fedimint_client.clone(),
    };

    let mut mint_service = Router::new()
        .route("/v1/keys", get(get_keys))
        .route("/v1/keysets", get(get_keysets))
        .route("/v1/keys/:keyset_id", get(get_keyset_pubkeys))
        .route("/v1/swap", post(post_swap))
        .route("/v1/mint/quote/bolt11", post(get_mint_bolt11_quote))
        .route(
            "/v1/mint/quote/bolt11/:quote_id",
            get(get_check_mint_bolt11_quote),
        )
        .route("/v1/mint/bolt11", post(post_mint_bolt11))
        .route("/v1/melt/quote/bolt11", post(get_melt_bolt11_quote))
        .route(
            "/v1/melt/quote/bolt11/:quote_id",
            get(get_check_melt_bolt11_quote),
        )
        .route("/v1/melt/bolt11", post(post_melt_bolt11))
        .route("/v1/checkstate", post(post_check))
        .route("/v1/info", get(get_mint_info))
        .route("/v1/restore", post(post_restore));

    let fedimint_service = Router::new()
        .route("/v1/mint/fedimint", post(post_mint_fedimint))
        .route("/v1/melt/fedimint", post(post_melt_fedimint));

    if fedimint_client.is_some() {
        mint_service = mint_service.merge(fedimint_service);
    }

    let app = mint_service
        .layer(CorsLayer::very_permissive().allow_headers([
            AUTHORIZATION,
            CONTENT_TYPE,
            ACCESS_CONTROL_ALLOW_CREDENTIALS,
            ACCESS_CONTROL_ALLOW_ORIGIN,
        ]))
        .with_state(state);

    let listener =
        tokio::net::TcpListener::bind(format!("{}:{}", listen_addr, listen_port)).await?;

    axum::serve(listener, app).await?;

    Ok(())
}

async fn handle_paid_invoice(mint: Arc<Mint>, request: Option<Bolt11Invoice>) -> Result<()> {
    let quotes: Vec<MintQuote> = mint.mint_quotes().await?;

    if let Some(invoice) = request {
        for quote in quotes {
            if quote.request.eq(&invoice.to_string()) {
                let q = MintQuote {
                    id: quote.id,
                    mint_url: quote.mint_url,
                    amount: quote.amount,
                    unit: quote.unit,
                    request: quote.request,
                    paid: true,
                    expiry: quote.expiry,
                };

                mint.update_mint_quote(q).await?;
            }
        }
    }

    Ok(())
}
#[derive(Clone)]
struct MintState {
    ln: Arc<dyn MintLightning<Err = cdk_lightning::Error> + Send + Sync>,
    mint: Mint,
    mint_url: String,
    fedimint_client: Option<ClientHandleArc>,
}

async fn get_keys(State(state): State<MintState>) -> Result<Json<KeysResponse>, Response> {
    let pubkeys = state.mint.pubkeys().await.map_err(into_response)?;

    Ok(Json(pubkeys))
}

async fn get_keyset_pubkeys(
    State(state): State<MintState>,
    Path(keyset_id): Path<Id>,
) -> Result<Json<KeysResponse>, Response> {
    let pubkeys = state
        .mint
        .keyset_pubkeys(&keyset_id)
        .await
        .map_err(into_response)?;

    Ok(Json(pubkeys))
}

async fn get_keysets(State(state): State<MintState>) -> Result<Json<KeysetResponse>, Response> {
    let mint = state.mint.keysets().await.map_err(into_response)?;

    Ok(Json(mint))
}

async fn get_mint_bolt11_quote(
    State(state): State<MintState>,
    Json(payload): Json<MintQuoteBolt11Request>,
) -> Result<Json<MintQuoteBolt11Response>, Response> {
    let amount = match payload.unit {
        CurrencyUnit::Sat => CDKLightningAmount::from_sat(payload.amount.into()),
        CurrencyUnit::Msat => CDKLightningAmount::from_msat(payload.amount.into()),
        _ => return Err(into_response(cdk::mint::error::Error::UnsupportedUnit)),
    };

    let expiry_time = unix_time() + 1800;

    let invoice = state
        .ln
        .create_invoice(amount, "".to_string(), expiry_time)
        .await
        .map_err(|_| into_response(Error::InvalidPaymentRequest))?;

    let quote = state
        .mint
        .new_mint_quote(
            state.mint_url.into(),
            invoice.to_string(),
            payload.unit,
            payload.amount,
            expiry_time,
        )
        .await
        .map_err(into_response)?;

    Ok(Json(quote.into()))
}

async fn get_check_mint_bolt11_quote(
    State(state): State<MintState>,
    Path(quote_id): Path<String>,
) -> Result<Json<MintQuoteBolt11Response>, Response> {
    let quote = state
        .mint
        .check_mint_quote(&quote_id)
        .await
        .map_err(into_response)?;

    Ok(Json(quote))
}

async fn post_mint_bolt11(
    State(state): State<MintState>,
    Json(payload): Json<MintBolt11Request>,
) -> Result<Json<MintBolt11Response>, Response> {
    let res = state
        .mint
        .process_mint_request(payload)
        .await
        .map_err(into_response)?;

    Ok(Json(res))
}

async fn get_melt_bolt11_quote(
    State(state): State<MintState>,
    Json(payload): Json<MeltQuoteBolt11Request>,
) -> Result<Json<MeltQuoteBolt11Response>, Response> {
    let amount = match payload.unit {
        CurrencyUnit::Sat => Amount::from(
            payload
                .request
                .amount_milli_satoshis()
                .ok_or(Error::InvoiceAmountUndefined)
                .map_err(into_response)?
                / 1000,
        ),
        CurrencyUnit::Msat => Amount::from(
            payload
                .request
                .amount_milli_satoshis()
                .ok_or(Error::InvoiceAmountUndefined)
                .map_err(into_response)?,
        ),
        _ => return Err(into_response(cdk::mint::error::Error::UnsupportedUnit)),
    };

    let fee_reserve = Amount::from(
        (state.mint.fee_reserve.percent_fee_reserve as f64 * u64::from(amount) as f64) as u64,
    );

    let quote = state
        .mint
        .new_melt_quote(
            payload.request.to_string(),
            payload.unit,
            amount,
            fee_reserve,
            unix_time() + 1800,
        )
        .await
        .map_err(into_response)?;

    Ok(Json(quote.into()))
}

async fn get_check_melt_bolt11_quote(
    State(state): State<MintState>,
    Path(quote_id): Path<String>,
) -> Result<Json<MeltQuoteBolt11Response>, Response> {
    let quote = state
        .mint
        .check_melt_quote(&quote_id)
        .await
        .map_err(into_response)?;

    Ok(Json(quote))
}

async fn post_melt_bolt11(
    State(state): State<MintState>,
    Json(payload): Json<MeltBolt11Request>,
) -> Result<Json<MeltBolt11Response>, Response> {
    let quote = state
        .mint
        .verify_melt_request(&payload)
        .await
        .map_err(into_response)?;

    let invoice = Bolt11Invoice::from_str(&quote.request)
        .map_err(|_| into_response(Error::InvalidPaymentRequest))?;

    let (preimage, amount_spent) = match state
        .mint
        .localstore
        .get_mint_quote_by_request(&quote.request)
        .await
        .unwrap()
    {
        Some(melt_quote) => {
            let mut melt_quote = melt_quote;
            melt_quote.paid = true;

            let amount = quote.amount;

            state.mint.update_mint_quote(melt_quote).await.unwrap();

            (None, amount)
        }
        None => {
            let pre = state
                .ln
                .pay_invoice(invoice, None, None)
                .await
                .map_err(|_| {
                    into_response(ErrorResponse::new(
                        cdk::error::ErrorCode::Unknown(999),
                        Some("Could not pay ln invoice".to_string()),
                        None,
                    ))
                })?;
            let amount = Amount::from(pre.total_spent.to_sat());

            (pre.payment_preimage, amount)
        }
    };

    let res = state
        .mint
        .process_melt_request(&payload, preimage, amount_spent)
        .await
        .map_err(into_response)?;

    Ok(Json(res))
}

async fn post_check(
    State(state): State<MintState>,
    Json(payload): Json<CheckStateRequest>,
) -> Result<Json<CheckStateResponse>, Response> {
    let state = state
        .mint
        .check_state(&payload)
        .await
        .map_err(into_response)?;

    Ok(Json(state))
}

async fn get_mint_info(State(state): State<MintState>) -> Result<Json<MintInfo>, Response> {
    Ok(Json(state.mint.mint_info().map_err(into_response)?))
}

async fn post_swap(
    State(state): State<MintState>,
    Json(payload): Json<SwapRequest>,
) -> Result<Json<SwapResponse>, Response> {
    let swap_response = state
        .mint
        .process_swap_request(payload)
        .await
        .map_err(into_response)?;
    Ok(Json(swap_response))
}

async fn post_restore(
    State(state): State<MintState>,
    Json(payload): Json<RestoreRequest>,
) -> Result<Json<RestoreResponse>, Response> {
    let restore_response = state.mint.restore(payload).await.map_err(into_response)?;

    Ok(Json(restore_response))
}

pub fn into_response<T>(error: T) -> Response
where
    T: Into<ErrorResponse>,
{
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json::<ErrorResponse>(error.into()),
    )
        .into_response()
}

async fn post_mint_fedimint(
    State(state): State<MintState>,
    Json(request): Json<MintFedimintRequest>,
) -> Result<Json<MintFedimintResponse>, Response> {
    let amount_msat = request.notes.total_amount();
    let client = state
        .fedimint_client
        .ok_or(into_response(Error::FedimintClientNotInitialized))?;
    let mint = client.get_first_module::<MintClientModule>();

    let operation_id = mint
        .reissue_external_notes(request.notes, ())
        .await
        .map_err(|e| {
            into_response(Error::CustomError(format!(
                "Failed to reissue notes: {}",
                e
            )))
        })?;

    let mut updates = mint
        .subscribe_reissue_external_notes(operation_id)
        .await
        .map_err(|e| {
            into_response(Error::CustomError(format!(
                "Failed to subscribe to reissue operation: {}",
                e
            )))
        })?
        .into_stream();

    while let Some(update) = updates.next().await {
        if let ReissueExternalNotesState::Failed(e) = update {
            return Err(into_response(Error::FedimintReissueFailed(e)));
        }
    }

    info!("Received {amount_msat} in fedimint ecash, minting {amount_msat} of cashu ecash");

    let mut signatures = vec![];

    for output in request.outputs {
        let signature = state.mint.blind_sign(&output).await.unwrap();
        signatures.push(signature.clone());

        state
            .mint
            .localstore
            .add_blinded_signature(output.blinded_secret, signature)
            .await
            .unwrap();
    }

    Ok(Json(MintFedimintResponse { signatures }))
}

async fn post_melt_fedimint(
    State(state): State<MintState>,
    Json(request): Json<MeltFedimintRequest>,
) -> Result<Json<MeltFedimintResponse>, Response> {
    let client = state
        .fedimint_client
        .ok_or(into_response(Error::FedimintClientNotInitialized))?;
    let mint = client.get_first_module::<MintClientModule>();

    state
        .mint
        .verify_melt_fedimint_request(&request)
        .await
        .map_err(into_response)?;

    let total_amount: cdk::Amount = request.inputs.iter().map(|p| p.amount.into()).sum();

    let (_, notes) = mint
        .spend_notes(
            FedimintAmount::from_sats(total_amount.into()),
            Duration::from_secs(60),
            true,
            (),
        )
        .await
        .map_err(|e| into_response(Error::CustomError(format!("Failed to spend notes: {}", e))))?;
    Ok(Json(MeltFedimintResponse { notes }))
}
