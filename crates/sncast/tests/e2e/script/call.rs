use crate::helpers::constants::{SCRIPTS_DIR, URL};
use crate::helpers::fixtures::copy_script_directory_to_tempdir;
use crate::helpers::runner::runner;
use indoc::indoc;
use shared::test_utils::output_assert::assert_stdout_contains;

#[tokio::test]
async fn test_happy_case() {
    let tempdir =
        copy_script_directory_to_tempdir(SCRIPTS_DIR.to_owned() + "/misc", Vec::<String>::new());

    let script_name = "call_happy";
    let args = vec!["script", "run", &script_name, "--url", URL];

    let snapbox = runner(&args).current_dir(tempdir.path());
    snapbox.assert().success().stdout_matches(indoc! {r"
        ...
        command: script run
        status: success
    "});
}

#[tokio::test]
async fn test_failing() {
    let tempdir =
        copy_script_directory_to_tempdir(SCRIPTS_DIR.to_owned() + "/misc", Vec::<String>::new());

    let script_name = "call_fail";
    let args = vec!["script", "run", &script_name, "--url", URL];

    let snapbox = runner(&args).current_dir(tempdir.path());
    let output = snapbox.assert().success();

    assert_stdout_contains(
        output,
        indoc! {r"
        command: script run
        message:[..]
            0x63616c6c206661696c6564 ('call failed')

        status: script panicked
        "},
    );
}

#[tokio::test]
async fn test_call_invalid_entry_point() {
    let tempdir =
        copy_script_directory_to_tempdir(SCRIPTS_DIR.to_owned() + "/call", Vec::<String>::new());

    let script_name = "invalid_entry_point";
    let args = vec!["script", "run", &script_name, "--url", URL];

    let snapbox = runner(&args).current_dir(tempdir.path());
    let output = snapbox.assert().success();

    assert_stdout_contains(
        output,
        indoc! {r#"
        ScriptCommandError::ProviderError(ProviderError::StarknetError(StarknetError::ContractError(ErrorData { msg: "Entry point EntryPointSelector([..]) not found in contract." })))
        command: script run
        status: success
        "#},
    );
}

#[tokio::test]
async fn test_call_invalid_address() {
    let tempdir =
        copy_script_directory_to_tempdir(SCRIPTS_DIR.to_owned() + "/call", Vec::<String>::new());

    let script_name = "invalid_address";
    let args = vec!["script", "run", &script_name, "--url", URL];

    let snapbox = runner(&args).current_dir(tempdir.path());
    let output = snapbox.assert().success();

    assert_stdout_contains(
        output,
        indoc! {r"
        ScriptCommandError::ProviderError(ProviderError::StarknetError(StarknetError::ContractNotFound(())))
        command: script run
        status: success
        "},
    );
}

#[tokio::test]
async fn test_call_invalid_calldata() {
    let tempdir =
        copy_script_directory_to_tempdir(SCRIPTS_DIR.to_owned() + "/call", Vec::<String>::new());

    let script_name = "invalid_calldata";
    let args = vec!["script", "run", &script_name, "--url", URL];

    let snapbox = runner(&args).current_dir(tempdir.path());
    let output = snapbox.assert().success();

    assert_stdout_contains(
        output,
        indoc! {r#"
        command: script run
        status: success
        ScriptCommandError::UnknownError(ErrorData { msg: "Failed to serialize input calldata "{ 0x12, 0x12, 0x12, 0x12, 0x12 }"" })
        ScriptCommandError::UnknownError(ErrorData { msg: "Failed to serialize input calldata "{ 0x12 }"" })
        "#},
    );
}
