use std::collections::HashMap;
use std::default::Default;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::cairo_runner::casm_run;
use crate::cairo_runner::casm_run::build_program_data;
use crate::cairo_runner::sierra_casm_runner::{initialize_vm, SierraCasmRunner};
use crate::compiled_runnable::ValidatedForkConfig;
use crate::gas::calculate_used_gas;
use crate::test_case_summary::{Single, TestCaseSummary};
use crate::{RunnerConfig, RunnerParams, TestCaseRunnable, CACHE_DIR};
use anyhow::{bail, ensure, Result};
use blockifier::execution::common_hints::ExecutionMode;
use blockifier::execution::entry_point::{
    CallEntryPoint, CallType, EntryPointExecutionContext, ExecutionResources,
};
use blockifier::execution::execution_utils::ReadOnlySegments;
use blockifier::execution::syscalls::hint_processor::SyscallHintProcessor;
use blockifier::state::cached_state::CachedState;
use blockifier::state::state_api::State;
use cairo_felt::Felt252;
use cairo_lang_casm::hints::Hint;
use cairo_lang_casm::instructions::Instruction;
use cairo_lang_runner::casm_run::hint_to_hint_params;
use cairo_lang_runner::{Arg, RunResult, RunnerError};
use cairo_lang_sierra::ids::GenericTypeId;
use cairo_lang_sierra_to_casm::compiler::CairoProgram;
use cairo_vm::serde::deserialize_program::HintParams;
use cairo_vm::types::relocatable::Relocatable;
use cairo_vm::vm::errors::vm_errors::VirtualMachineError;
use cairo_vm::vm::runners::cairo_runner::CairoRunner;
use cairo_vm::vm::vm_core::VirtualMachine;
use camino::Utf8Path;
use cheatnet::constants as cheatnet_constants;
use cheatnet::forking::state::ForkStateReader;
use cheatnet::runtime_extensions::call_to_blockifier_runtime_extension::CallToBlockifierExtension;
use cheatnet::runtime_extensions::cheatable_starknet_runtime_extension::CheatableStarknetRuntimeExtension;
use cheatnet::runtime_extensions::forge_runtime_extension::get_all_execution_resources;
use cheatnet::runtime_extensions::forge_runtime_extension::{ForgeExtension, ForgeRuntime};
use cheatnet::runtime_extensions::io_runtime_extension::IORuntimeExtension;
use cheatnet::state::{BlockInfoReader, CheatnetBlockInfo, CheatnetState, ExtendedStateReader};
use itertools::chain;
use runtime::{ExtendedRuntime, StarknetRuntime};
use starknet::core::types::BlockId;
use starknet::core::utils::get_selector_from_name;
use starknet_api::core::PatriciaKey;
use starknet_api::core::{ContractAddress, EntryPointSelector};
use starknet_api::deprecated_contract_class::EntryPointType;
use starknet_api::hash::StarkHash;
use starknet_api::patricia_key;
use starknet_api::transaction::Calldata;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

/// Builds `hints_dict` required in `cairo_vm::types::program::Program` from instructions.
fn build_hints_dict<'b>(
    instructions: impl Iterator<Item = &'b Instruction>,
) -> (HashMap<usize, Vec<HintParams>>, HashMap<String, Hint>) {
    let mut hints_dict: HashMap<usize, Vec<HintParams>> = HashMap::new();
    let mut string_to_hint: HashMap<String, Hint> = HashMap::new();

    let mut hint_offset = 0;

    for instruction in instructions {
        if !instruction.hints.is_empty() {
            // Register hint with string for the hint processor.
            for hint in &instruction.hints {
                string_to_hint.insert(format!("{hint:?}"), hint.clone());
            }
            // Add hint, associated with the instruction offset.
            hints_dict.insert(
                hint_offset,
                instruction.hints.iter().map(hint_to_hint_params).collect(),
            );
        }
        hint_offset += instruction.body.op_size();
    }
    (hints_dict, string_to_hint)
}

pub fn run_test(
    case: Arc<TestCaseRunnable>,
    casm_program: Arc<CairoProgram>,
    test_details: Arc<TestDetails>,
    runner_config: Arc<RunnerConfig>,
    runner_params: Arc<RunnerParams>,
    send: Sender<()>,
) -> JoinHandle<Result<TestCaseSummary<Single>>> {
    tokio::task::spawn_blocking(move || {
        // Due to the inability of spawn_blocking to be abruptly cancelled,
        // a channel is used to receive information indicating
        // that the execution of the task is no longer necessary.
        if send.is_closed() {
            return Ok(TestCaseSummary::Skipped {});
        }
        let run_result = run_test_case(
            vec![],
            &case,
            &casm_program,
            &test_details,
            &runner_config,
            &runner_params,
        );

        // TODO: code below is added to fix snforge tests
        // remove it after improve exit-first tests
        // issue #1043
        if send.is_closed() {
            return Ok(TestCaseSummary::Skipped {});
        }

        extract_test_case_summary(run_result, &case, vec![])
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_fuzz_test(
    args: Vec<Felt252>,
    case: Arc<TestCaseRunnable>,
    casm_program: Arc<CairoProgram>,
    test_details: Arc<TestDetails>,
    runner_config: Arc<RunnerConfig>,
    runner_params: Arc<RunnerParams>,
    send: Sender<()>,
    fuzzing_send: Sender<()>,
) -> JoinHandle<Result<TestCaseSummary<Single>>> {
    tokio::task::spawn_blocking(move || {
        // Due to the inability of spawn_blocking to be abruptly cancelled,
        // a channel is used to receive information indicating
        // that the execution of the task is no longer necessary.
        if send.is_closed() | fuzzing_send.is_closed() {
            return Ok(TestCaseSummary::Skipped {});
        }

        let run_result = run_test_case(
            args.clone(),
            &case,
            &casm_program,
            &test_details,
            &runner_config,
            &runner_params,
        );

        // TODO: code below is added to fix snforge tests
        // remove it after improve exit-first tests
        // issue #1043
        if send.is_closed() {
            return Ok(TestCaseSummary::Skipped {});
        }

        extract_test_case_summary(run_result, &case, args)
    })
}

fn build_context(block_info: CheatnetBlockInfo) -> EntryPointExecutionContext {
    let block_context = cheatnet_constants::build_block_context(block_info);
    let account_context = cheatnet_constants::build_transaction_context();

    EntryPointExecutionContext::new(
        &block_context,
        &account_context,
        ExecutionMode::Execute,
        false,
    )
    .unwrap()
}

fn build_syscall_handler<'a>(
    blockifier_state: &'a mut dyn State,
    string_to_hint: &'a HashMap<String, Hint>,
    execution_resources: &'a mut ExecutionResources,
    context: &'a mut EntryPointExecutionContext,
) -> SyscallHintProcessor<'a> {
    let test_selector = get_selector_from_name("TEST_CONTRACT_SELECTOR").unwrap();
    let entry_point_selector =
        EntryPointSelector(StarkHash::new(test_selector.to_bytes_be()).unwrap());
    let entry_point = CallEntryPoint {
        class_hash: None,
        code_address: Some(ContractAddress(patricia_key!(
            cheatnet_constants::TEST_ADDRESS
        ))),
        entry_point_type: EntryPointType::External,
        entry_point_selector,
        calldata: Calldata(Arc::new(vec![])),
        storage_address: ContractAddress(patricia_key!(cheatnet_constants::TEST_ADDRESS)),
        caller_address: ContractAddress::default(),
        call_type: CallType::Call,
        initial_gas: u64::MAX,
    };

    SyscallHintProcessor::new(
        blockifier_state,
        execution_resources,
        context,
        // This segment is created by SierraCasmRunner
        Relocatable {
            segment_index: 10,
            offset: 0,
        },
        entry_point,
        string_to_hint,
        ReadOnlySegments::default(),
    )
}

#[derive(Debug)]
pub struct RunResultWithInfo {
    pub(crate) run_result: Result<RunResult, RunnerError>,
    pub(crate) gas_used: u128,
}

// TODO merge this into test-collector's `TestCase`
pub struct TestDetails {
    pub entry_point_offset: usize,
    pub parameter_types: Vec<(GenericTypeId, i16)>,
    pub return_types: Vec<(GenericTypeId, i16)>,
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub fn run_test_case(
    args: Vec<Felt252>,
    case: &TestCaseRunnable,
    casm_program: &CairoProgram,
    test_details: &TestDetails,
    runner_config: &Arc<RunnerConfig>,
    runner_params: &Arc<RunnerParams>,
) -> Result<RunResultWithInfo> {
    ensure!(
        case.available_gas.is_none(),
        "\n    Attribute `available_gas` is not supported\n"
    );

    let initial_gas = usize::MAX;
    let runner_args: Vec<Arg> = args.into_iter().map(Arg::Value).collect();

    let (entry_code, builtins) = SierraCasmRunner::create_entry_code_from_params(
        &test_details.parameter_types,
        &runner_args,
        initial_gas,
        casm_program.debug_info.sierra_statement_info[test_details.entry_point_offset].code_offset,
    )
    .unwrap();
    let footer = SierraCasmRunner::create_code_footer();
    let instructions = chain!(
        entry_code.iter(),
        casm_program.instructions.iter(),
        footer.iter()
    );
    let (hints_dict, string_to_hint) = build_hints_dict(instructions.clone());

    let mut state_reader = ExtendedStateReader {
        dict_state_reader: cheatnet_constants::build_testing_state(),
        fork_state_reader: get_fork_state_reader(&runner_config.workspace_root, &case.fork_config),
    };
    let block_info = state_reader.get_block_info()?;

    let mut context = build_context(block_info);
    let mut execution_resources = ExecutionResources::default();
    let mut blockifier_state = CachedState::from(state_reader);
    let syscall_handler = build_syscall_handler(
        &mut blockifier_state,
        &string_to_hint,
        &mut execution_resources,
        &mut context,
    );

    let mut cheatnet_state = CheatnetState {
        block_info,
        ..Default::default()
    };

    let cheatable_runtime = ExtendedRuntime {
        extension: CheatableStarknetRuntimeExtension {
            cheatnet_state: &mut cheatnet_state,
        },
        extended_runtime: StarknetRuntime {
            hint_handler: syscall_handler,
        },
    };

    let io_runtime = ExtendedRuntime {
        extension: IORuntimeExtension {
            lifetime: &PhantomData,
        },
        extended_runtime: cheatable_runtime,
    };

    let call_to_blockifier_runtime = ExtendedRuntime {
        extension: CallToBlockifierExtension {
            lifetime: &PhantomData,
        },
        extended_runtime: io_runtime,
    };

    let forge_extension = ForgeExtension {
        environment_variables: &runner_params.environment_variables,
        contracts: &runner_params.contracts,
    };

    let mut forge_runtime = ExtendedRuntime {
        extension: forge_extension,
        extended_runtime: call_to_blockifier_runtime,
    };

    // copied from casm_run
    let mut vm = VirtualMachine::new(true);
    let data = build_program_data(instructions);
    let data_len = data.len();
    // end of copied code

    let mut runner = casm_run::build_runner(data, builtins, hints_dict)?;
    let run_result = match casm_run::run_function_with_runner(
        &mut vm,
        data_len,
        initialize_vm,
        &mut forge_runtime,
        &mut runner,
    ) {
        Ok(()) => {
            finalize(
                &mut vm,
                &runner,
                &mut forge_runtime
                    .extended_runtime
                    .extended_runtime
                    .extended_runtime
                    .extended_runtime
                    .hint_handler,
                0,
                2,
            );

            let cells = runner.relocated_memory;
            let ap = vm.get_relocated_trace().unwrap().last().unwrap().ap;

            let (results_data, gas_counter) =
                SierraCasmRunner::get_results_data(&test_details.return_types, &cells, ap);
            assert_eq!(results_data.len(), 1);
            let (_, values) = results_data[0].clone();
            let value = SierraCasmRunner::handle_main_return_value(
                // Here we assume that all test either panic or do not return any value
                // This is true for all test right now, but in case it changes
                // this logic will need to be updated
                Some(0),
                values,
                &cells,
            );
            Ok(RunResult {
                gas_counter,
                memory: cells,
                value,
            })
        }
        Err(err) => Err(RunnerError::CairoRunError(err)),
    };

    let block_context = get_context(&forge_runtime).block_context.clone();
    let execution_resources = get_all_execution_resources(forge_runtime);

    let gas = calculate_used_gas(&block_context, &mut blockifier_state, &execution_resources);

    Ok(RunResultWithInfo {
        run_result,
        gas_used: gas,
    })
}

fn extract_test_case_summary(
    run_result: Result<RunResultWithInfo>,
    case: &TestCaseRunnable,
    args: Vec<Felt252>,
) -> Result<TestCaseSummary<Single>> {
    match run_result {
        Ok(result_with_info) => {
            match result_with_info.run_result {
                Ok(run_result) => Ok(TestCaseSummary::from_run_result_and_info(
                    run_result,
                    case,
                    args,
                    result_with_info.gas_used,
                )),
                // CairoRunError comes from VirtualMachineError which may come from HintException that originates in TestExecutionSyscallHandler
                Err(RunnerError::CairoRunError(error)) => Ok(TestCaseSummary::Failed {
                    name: case.name.clone(),
                    msg: Some(format!(
                        "\n    {}\n",
                        error.to_string().replace(" Custom Hint Error: ", "\n    ")
                    )),
                    arguments: args,
                    test_statistics: (),
                }),
                Err(err) => bail!(err),
            }
        }
        // `ForkStateReader.get_block_info`, `get_fork_state_reader` may return an error
        // unsupported `available_gas` attribute may be specified
        Err(error) => Ok(TestCaseSummary::Failed {
            name: case.name.clone(),
            msg: Some(error.to_string()),
            arguments: args,
            test_statistics: (),
        }),
    }
}

fn get_fork_state_reader(
    workspace_root: &Utf8Path,
    fork_config: &Option<ValidatedForkConfig>,
) -> Option<ForkStateReader> {
    fork_config
        .as_ref()
        .map(|ValidatedForkConfig { url, block_number }| {
            ForkStateReader::new(
                url.clone(),
                BlockId::Number(block_number.0),
                Some(workspace_root.join(CACHE_DIR).as_ref()),
            )
        })
}

fn get_context<'a>(runtime: &'a ForgeRuntime) -> &'a EntryPointExecutionContext {
    runtime
        .extended_runtime
        .extended_runtime
        .extended_runtime
        .extended_runtime
        .hint_handler
        .context
}

fn finalize(
    vm: &mut VirtualMachine,
    runner: &CairoRunner,
    syscall_handler: &mut SyscallHintProcessor<'_>,
    n_total_args: usize,
    program_extra_data_length: usize,
) {
    let program_start_ptr = runner
        .program_base
        .expect("The `program_base` field should be initialized after running the entry point.");
    let program_end_ptr = (program_start_ptr + runner.get_program().data_len()).unwrap();
    vm.mark_address_range_as_accessed(program_end_ptr, program_extra_data_length)
        .unwrap();

    let initial_fp = runner
        .get_initial_fp()
        .expect("The `initial_fp` field should be initialized after running the entry point.");
    // When execution starts the stack holds the EP arguments + [ret_fp, ret_pc].
    let args_ptr = (initial_fp - (n_total_args + 2)).unwrap();
    vm.mark_address_range_as_accessed(args_ptr, n_total_args)
        .unwrap();
    syscall_handler
        .read_only_segments
        .mark_as_accessed(vm)
        .unwrap();

    let vm_resources_without_inner_calls = runner
        .get_execution_resources(vm)
        .map_err(VirtualMachineError::TracerError)
        .unwrap()
        .filter_unused_builtins();
    syscall_handler.resources.vm_resources += &vm_resources_without_inner_calls;
}
