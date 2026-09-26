//! Minimal smoke test against a real provider:
//!   ANTHROPIC_API_KEY=... cargo run --example ping -- --provider anthropic
//!   OPENAI_API_KEY=... cargo run --example ping -- --provider openai

use tack_ai::*;

#[tokio::main]
async fn main() {
    let provider_name = std::env::args()
        .skip_while(|a| a != "--provider")
        .nth(1)
        .unwrap_or_else(|| "anthropic".to_string());

    let model = match provider_name.as_str() {
        "anthropic" => Model {
            id: std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-haiku-4-5".to_string()),
            name: "Claude".to_string(),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            base_url: std::env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com".to_string())
                .trim_end_matches('/')
                .to_string(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![InputKind::Text],
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 1024,
            sampling_params: None,
            headers: None,
            compat: None,
        },
        "openai" => Model {
            id: "gpt-5-mini".to_string(),
            name: "GPT mini".to_string(),
            api: "openai-completions".to_string(),
            provider: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![InputKind::Text],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 1024,
            sampling_params: None,
            headers: None,
            compat: None,
        },
        other => {
            eprintln!("unknown provider {other:?} (supported: anthropic, openai)");
            std::process::exit(2);
        }
    };

    let Some(api_key) = env_keys::get_env_api_key(&model.provider) else {
        eprintln!("no API key found in environment for {}", model.provider);
        std::process::exit(2);
    };

    let context = Context {
        system_prompt: None,
        messages: vec![Message::user("Say hi in exactly one word.")],
        tools: vec![],
    };
    let options = StreamOptions {
        api_key: Some(api_key),
        ..Default::default()
    };

    let provider = provider_for(&model).expect("adapter exists");
    let mut stream = provider.stream(&model, &context, options);
    while let Some(event) = stream.next().await {
        match &event {
            AssistantMessageEvent::TextDelta { delta, .. } => print!("{delta}"),
            AssistantMessageEvent::Done { reason, .. } => eprintln!("\n[done: {reason:?}]"),
            AssistantMessageEvent::Error { error, .. } => {
                eprintln!("\n[error: {:?}]", error.error_message)
            }
            _ => {}
        }
        if event.is_terminal() {
            break;
        }
    }
    let message = stream.result().await;
    eprintln!(
        "[usage: in={} out={}]",
        message.usage.input, message.usage.output
    );
}
