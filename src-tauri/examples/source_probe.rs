// Read-only diagnostic of the currently selected media source. Never controls playback.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let controller = nowplaying::MediaController::new().await?;
    if let Some(session) = controller.current().await? {
        println!("{}", serde_json::to_string_pretty(&session.source)?);
        let fallback = tune_love::source::fallback_name(
            &session.source.id,
            session.source.name.as_deref(),
        );
        let identity = tokio::task::spawn_blocking(move || {
            tune_love::source::resolve(&session.source.id, &fallback)
        })
        .await?;
        println!(
            "{}",
            serde_json::json!({ "name": identity.name, "iconBytes": identity.icon_data_url.as_ref().map_or(0, String::len), "png": identity.icon_data_url.as_ref().is_some_and(|s| s.starts_with("data:image/png;base64,")) })
        );
    } else {
        println!("No current media session");
    }
    Ok(())
}
