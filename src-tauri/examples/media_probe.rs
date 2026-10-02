// Read-only integration probe. Does not control any player or host.
#[tokio::main]
async fn main() {
    let state = helper_now_playing::media::MediaState::default();
    let probe = async {
        for _ in 0..12 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let snapshot = state.snapshot();
            if snapshot.status != "loading" {
                let mut json = serde_json::to_value(&snapshot).unwrap();
                if let Some(cover) = json.pointer_mut("/track/artworkDataUrl") {
                    if let Some(value) = cover.as_str() {
                        *cover = serde_json::json!(format!(
                            "{}... ({} chars)",
                            &value[..value.len().min(50)],
                            value.len()
                        ));
                    }
                }
                println!("{}", serde_json::to_string_pretty(&json).unwrap());
            }
        }
    };
    tokio::select! { _ = helper_now_playing::media::watch(state.clone()) => {}, _ = probe => {} }
}
