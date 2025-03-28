use crate::kumod::{DaemonWithMaildir, MailGenParams};
use kumo_api_types::SuspendV1Response;
use kumo_log_types::RecordType::{Reception, TransientFailure};
use std::time::Duration;

#[tokio::test]
async fn rebind_event_missing() -> anyhow::Result<()> {
    let mut daemon = DaemonWithMaildir::start().await?;
    let mut client = daemon.smtp_client().await?;

    let status: SuspendV1Response = daemon
        .kcli_json(["suspend", "--domain", "example.com", "--reason", "testing"])
        .await?;
    println!("kcli status: {status:?}");

    let response = MailGenParams {
        recip: Some("allow@example.com"),
        ..Default::default()
    }
    .send(&mut client)
    .await?;
    eprintln!("{response:?}");
    anyhow::ensure!(response.code == 250);

    daemon
        .wait_for_source_summary(
            |summary| summary.get(&Reception).copied().unwrap_or(0) > 0,
            Duration::from_secs(50),
        )
        .await;

    daemon
        .kcli([
            "rebind",
            "--domain",
            "example.com",
            "--reason",
            "testing",
            "--trigger-rebind-event",
            "--data",
            "{\"queue\":\"rebound.com\"}",
        ])
        .await?;

    daemon
        .wait_for_source_summary(
            |summary| summary.get(&TransientFailure).copied().unwrap_or(0) > 0,
            Duration::from_secs(50),
        )
        .await;

    daemon.stop_both().await?;
    let delivery_summary = daemon.dump_logs().await?;
    assert!(
        delivery_summary
            .source_counts
            .get(&TransientFailure)
            .copied()
            .unwrap_or(0)
            > 0
    );
    Ok(())
}
