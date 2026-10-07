//! Tests for the Sarvam provider surface.

use super::*;

#[tokio::test]
async fn validates_before_connecting() {
    let provider = SarvamCascade::new(" ");
    assert_eq!(provider.id(), "sarvam");
    assert!(provider.capabilities().tools);
    assert!(matches!(
        provider.connect(LiveConfig::new()).await,
        Err(Error::InvalidConfig(_))
    ));
    let provider = SarvamCascade::new("k");
    let mut config = LiveConfig::new();
    config.input_format = AudioFormat::pcm16(24_000);
    assert!(matches!(
        provider.connect(config).await,
        Err(Error::InvalidConfig(_))
    ));
    let provider = SarvamCascade::new("k").with_endpoints(SarvamEndpoints {
        stt: "nope".into(),
        ..SarvamEndpoints::default()
    });
    assert!(matches!(
        provider.connect(LiveConfig::new()).await,
        Err(Error::InvalidConfig(_))
    ));
}

#[test]
fn debug_output_hides_the_key() {
    let debug = format!("{:?}", SarvamCascade::new("secret-key"));
    assert!(!debug.contains("secret-key"));
    assert!(debug.contains("api.sarvam.ai"));
}
