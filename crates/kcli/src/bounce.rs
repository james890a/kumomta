use clap::Parser;
use kumo_api_types::{BounceV1Request, BounceV1Response};
use reqwest::Url;
use std::time::Duration;

#[derive(Debug, Parser)]
/// Administratively bounce messages in matching queues.
///
/// Each individual message that is bounced will generate a log
/// record capturing the event and then be removed from the spool.
///
/// Make sure that you mean it, as there is no going back!
///
/// The bounce will be applied immediately to queued messages,
/// and the directive will remain in effect for the duration
/// specified, causing newly received messages or messages
/// that were in a transient state at the time the directive
/// was received, to also be bounced as they are placed
/// back into the matching queue(s).
pub struct BounceCommand {
    /// The domain name to match.
    /// If omitted, any domains will match!
    #[arg(long)]
    domain: Option<String>,

    /// The campaign name to match.
    /// If omitted, any campaigns will match!
    #[arg(long)]
    campaign: Option<String>,

    /// The tenant name to match.
    /// If omitted, any tenant will match!
    #[arg(long)]
    tenant: Option<String>,

    /// The reason to log in the delivery logs
    #[arg(long)]
    reason: String,

    /// Purge all queues.
    #[arg(long)]
    everything: bool,

    /// The duration over which matching messages will continue to bounce.
    /// The default is '5m'.
    #[arg(long, value_parser=humantime::parse_duration)]
    duration: Option<Duration>,
}

impl BounceCommand {
    pub async fn run(&self, endpoint: &Url) -> anyhow::Result<()> {
        if self.domain.is_none() && self.campaign.is_none() && self.tenant.is_none() {
            if !self.everything {
                anyhow::bail!(
                    "No domain, campaign or tenant was specified. \
                     Use --everything if you intend to purge all queues"
                );
            }
        }

        let result: BounceV1Response = crate::post(
            endpoint.join("/api/admin/bounce/v1")?,
            &BounceV1Request {
                campaign: self.campaign.clone(),
                domain: self.domain.clone(),
                tenant: self.tenant.clone(),
                reason: self.reason.clone(),
                duration: self.duration.clone(),
            },
        )
        .await?
        .json()
        .await?;

        println!("{}", serde_json::to_string_pretty(&result)?);

        Ok(())
    }
}
