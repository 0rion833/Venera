//! Price math, token counting, and the offline `--test` path end to end.
//!
//! Prices here are the real ProxyAPI list prices for `openai/gpt-6-luna` (RUB, VAT
//! included): 30/150 for input/output, 3/37.50 for cache read/write, doubled input and
//! 1.5x output above 272K prompt tokens.

use std::error::Error;

use venera::config::Config;
use venera::core::cost::{Budget, CostCounter, format_rub, turn_cost};
use venera::core::prompt;
use venera::provider::tokenizer;
use venera::provider::usage::Usage;

#[test]
fn builtin_config_parses_and_validates() -> Result<(), Box<dyn Error>> {
    let config = Config::builtin()?;
    config.validate()?;

    let resolved = config.resolve_model(None, None)?;
    assert_eq!(resolved.vendor_name, "proxyapi");
    assert_eq!(resolved.model.id, "openai/gpt-6-luna");
    assert_eq!(resolved.model.context_window, 1_050_000);
    assert_eq!(
        resolved.vendor.chat_url(),
        "https://api.proxyapi.ru/v1/chat/completions"
    );
    Ok(())
}

#[test]
fn model_lookup_accepts_provider_ids_and_bare_names() -> Result<(), Box<dyn Error>> {
    let config = Config::builtin()?;

    for name in ["gpt-6-luna", "openai/gpt-6-luna", "GPT-6-Luna"] {
        let resolved = config.resolve_model(Some("proxyapi"), Some(name))?;
        assert_eq!(resolved.model_key, "gpt-6-luna");
    }
    Ok(())
}

#[test]
fn unknown_model_lists_what_is_available() -> Result<(), Box<dyn Error>> {
    let config = Config::builtin()?;
    let error = config
        .resolve_model(Some("proxyapi"), Some("nope"))
        .expect_err("unknown model must fail");
    let message = error.to_string();

    assert!(message.contains("gpt-6-luna"), "got: {message}");
    assert!(message.contains("proxyapi"), "got: {message}");
    Ok(())
}

#[test]
fn unknown_fields_are_rejected() {
    let json = r#"{"vendors":{},"typo":1}"#;
    assert!(serde_json::from_str::<Config>(json).is_err());
}

#[test]
fn pricing_matches_the_proxyapi_price_list() -> Result<(), Box<dyn Error>> {
    let config = Config::builtin()?;
    let model = config
        .resolve_model(Some("proxyapi"), Some("gpt-6-luna"))?
        .model;

    // 100K prompt tokens stay in the normal tier: 30 руб. per 1M input, 150 руб. per 1M output.
    let usage = Usage {
        input: 100_000,
        output: 100_000,
        ..Usage::default()
    };
    let cost = turn_cost(&model.price, &usage);

    assert!(!cost.long_context);
    assert_eq!(cost.input_rub, 3.0);
    assert_eq!(cost.output_rub, 15.0);
    assert_eq!(cost.total_rub(), 18.0);
    Ok(())
}

#[test]
fn long_context_tier_starts_above_272k_prompt_tokens() -> Result<(), Box<dyn Error>> {
    let config = Config::builtin()?;
    let model = config
        .resolve_model(Some("proxyapi"), Some("gpt-6-luna"))?
        .model;

    let at_threshold = Usage {
        input: 272_000,
        ..Usage::default()
    };
    let above_threshold = Usage {
        input: 272_001,
        ..Usage::default()
    };

    let normal = turn_cost(&model.price, &at_threshold);
    let long = turn_cost(&model.price, &above_threshold);

    assert!(!normal.long_context);
    assert!(long.long_context);
    assert_eq!(normal.total_rub(), 272_000.0 / 1_000_000.0 * 30.0);
    assert_eq!(long.total_rub(), 272_001.0 / 1_000_000.0 * 60.0);
    Ok(())
}

#[test]
fn greeting_cost_is_positive_and_tiny() -> Result<(), Box<dyn Error>> {
    let config = Config::builtin()?;
    let model = config.resolve_model(None, None)?.model;

    let bare = prompt::count(model.encoding, "привет", false);
    assert!(bare.user >= 1, "the greeting must tokenize to something");
    assert_eq!(bare.framing, 6);

    let full = prompt::count(model.encoding, "привет", true);
    assert_eq!(full.framing, 9);
    assert!(full.system > 0);
    assert!(full.digest > 0);
    assert!(full.total() > bare.total());

    let usage = Usage {
        input: full.total() as u64,
        ..Usage::default()
    };
    let cost = turn_cost(&model.price, &usage);

    assert!(cost.total_rub() > 0.0);
    assert!(cost.total_rub() < 0.01, "got {}", cost.total_rub());
    Ok(())
}

#[test]
fn tokenizer_is_stable_and_encoding_aware() {
    let russian = tokenizer::count(venera::config::Encoding::O200kBase, "привет");
    assert!(russian >= 1);
    assert_eq!(
        russian,
        tokenizer::count(venera::config::Encoding::O200kBase, "привет")
    );

    let ascii = tokenizer::count(venera::config::Encoding::O200kBase, "hello");
    assert!(ascii >= 1);
}

#[test]
fn session_counter_tracks_the_rub_budget() -> Result<(), Box<dyn Error>> {
    let config = Config::builtin()?;
    let model = config.resolve_model(None, None)?.model;
    let mut counter = CostCounter::new(config.limits.session_budget_rub);

    let usage = Usage {
        output: 1_000_000,
        ..Usage::default()
    };
    let cost = turn_cost(&model.price, &usage);

    assert_eq!(counter.add(&cost), Budget::Within);
    assert_eq!(counter.spent_rub(), 150.0);
    assert_eq!(counter.remaining_rub(), Some(350.0));
    assert_eq!(format_rub(counter.spent_rub()), "150 руб.");
    Ok(())
}

#[test]
fn schema_and_defaults_agree() -> Result<(), Box<dyn Error>> {
    let schema = venera::config::schema::schema_json()?;
    assert_eq!(schema["type"], "object");

    let config: serde_json::Value = serde_json::from_str(venera::config::DEFAULT_CONFIG)?;
    let vendors = config["vendors"]
        .as_object()
        .expect("defaults must declare vendors");
    assert!(vendors.contains_key("proxyapi"));

    let reasoning = &config["vendors"]["proxyapi"]["models"]["gpt-6-luna"]["reasoning"];
    assert_eq!(reasoning, "medium");
    assert_eq!(
        schema["definitions"]["model"]["properties"]["reasoning"]["enum"][2],
        "medium"
    );
    Ok(())
}

#[test]
fn cli_test_command_prints_tokens_and_rub() -> Result<(), Box<dyn Error>> {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_venera"))
        .args([
            "--vendor",
            "proxyapi",
            "--model",
            "gpt-6-luna",
            "--test",
            "привет",
        ])
        .output()?;

    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("openai/gpt-6-luna"), "got: {stdout}");
    assert!(stdout.contains("o200k_base"), "got: {stdout}");
    assert!(stdout.contains("from config"), "got: {stdout}");
    assert!(stdout.contains("reasoning     medium"), "got: {stdout}");
    assert!(stdout.contains("prompt        system"), "got: {stdout}");
    assert!(stdout.contains("ИТОГО"), "got: {stdout}");
    assert!(stdout.contains("руб."), "got: {stdout}");
    assert!(stdout.contains("normal"), "got: {stdout}");
    Ok(())
}

#[test]
fn cli_bare_prompt_skips_the_system_message() -> Result<(), Box<dyn Error>> {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_venera"))
        .args(["--bare", "--test", "привет"])
        .output()?;

    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("bare: no system prompt"), "got: {stdout}");
    assert!(
        stdout.contains("user 2 + framing 6 = 8 tokens"),
        "got: {stdout}"
    );
    Ok(())
}

#[test]
fn cli_reasoning_flag_overrides_the_model_default() -> Result<(), Box<dyn Error>> {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_venera"))
        .args(["--reasoning", "high", "--test", "hi"])
        .output()?;

    let stdout = String::from_utf8(output.stdout)?;
    assert!(output.status.success());
    assert!(stdout.contains("reasoning     high"), "got: {stdout}");
    assert!(stdout.contains("billed as output"), "got: {stdout}");

    let off = std::process::Command::new(env!("CARGO_BIN_EXE_venera"))
        .args(["--reasoning", "off", "--test", "hi"])
        .output()?;
    let off_stdout = String::from_utf8(off.stdout)?;
    assert!(
        off_stdout.contains("reasoning     off"),
        "got: {off_stdout}"
    );
    Ok(())
}

#[test]
fn cli_models_filter_keeps_the_curated_model() -> Result<(), Box<dyn Error>> {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_venera"))
        .args(["models", "--filter", "luna", "--limit", "5"])
        .output()?;

    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("CURATED"), "got: {stdout}");
    assert!(stdout.contains("gpt-6-luna"), "got: {stdout}");
    assert!(stdout.contains("30,00 руб./1M"), "got: {stdout}");
    Ok(())
}

#[test]
fn cli_rejects_an_unknown_model() -> Result<(), Box<dyn Error>> {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_venera"))
        .args(["--model", "definitely-not-a-model", "--test", "hi"])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("unknown model"), "got: {stderr}");
    assert!(stderr.contains("gpt-6-luna"), "got: {stderr}");
    Ok(())
}
