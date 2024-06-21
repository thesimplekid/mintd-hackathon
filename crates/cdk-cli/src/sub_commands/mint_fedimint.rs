use anyhow::Result;
use cdk::amount::SplitTarget;
use cdk::url::UncheckedUrl;
use cdk::wallet::Wallet;
use clap::Args;

#[derive(Args)]
pub struct MintFedimintSubCommand {
    /// Mint url
    #[arg(short, long)]
    mint_url: UncheckedUrl,
    /// Fedimint Notes
    #[arg(short, long)]
    notes: String,
}

pub async fn mint_fedimint(
    wallet: Wallet,
    sub_command_args: &MintFedimintSubCommand,
) -> Result<()> {
    let mint_url = sub_command_args.mint_url.clone();

    let receive_amount = wallet
        .mint_fedimint(
            &mint_url,
            &sub_command_args.notes,
            SplitTarget::default(),
            None,
        )
        .await?;

    println!("Received {receive_amount} from mint {mint_url}");

    Ok(())
}
