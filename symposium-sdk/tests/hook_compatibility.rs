//! Downstream source contracts retained throughout SDK 0.1.
use std::process::ExitCode;

use symposium_sdk::hook::{self, HookHandler, Input, Output, PreToolUseInput, PreToolUseOutput};

struct LegacyHandler;
impl HookHandler for LegacyHandler {
    async fn pre_tool_use(&self, _: &PreToolUseInput) -> hook::anyhow::Result<PreToolUseOutput> {
        Ok(PreToolUseOutput::default())
    }
}

fn legacy_entry_point(handler: LegacyHandler) -> ExitCode {
    hook::run::<LegacyHandler>(handler)
}

async fn legacy_dispatch(handler: &LegacyHandler, input: &Input) -> hook::anyhow::Result<Output> {
    hook::default_handle_event::<LegacyHandler>(handler, input).await
}

#[test]
fn original_trait_methods_reexport_and_generic_arity_remain_supported() {
    let _: fn(LegacyHandler) -> ExitCode = legacy_entry_point;
    let _ = legacy_dispatch;
    let error = hook::anyhow::anyhow!("legacy handler failure");
    let _: hook::HarnessError<hook::anyhow::Error> = hook::HarnessError::Handler { source: error };
}

struct BorrowedError<'a>(&'a str);
struct BorrowedHandler<'a>(&'a str);

impl<'a> HookHandler<BorrowedError<'a>> for BorrowedHandler<'a> {
    async fn pre_tool_use(
        &self,
        _: &PreToolUseInput,
    ) -> Result<PreToolUseOutput, BorrowedError<'a>> {
        Err(BorrowedError(self.0))
    }
}

#[test]
fn typed_dispatch_accepts_borrowed_errors_without_error_or_static_bounds() {
    let message = String::from("borrowed error");
    let handler = BorrowedHandler(&message);
    let input: Input = serde_json::from_str(r#"{"PreToolUse":{"tool_name":"Bash"}}"#).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let error = runtime
        .block_on(hook::try_default_handle_event(&handler, &input))
        .err()
        .unwrap();
    assert_eq!(error.0, message);
}
