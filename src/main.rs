mod cli;
mod quantum_annealing;

use crate::quantum_annealing::dwave_api::sample_quantum_annealer;
use unicorn::unicorn::bitblasting::bitblast_model;
use unicorn::unicorn::bitblasting_dimacs::write_dimacs_model;
use unicorn::unicorn::bitblasting_printer::write_btor2_model;
use unicorn::unicorn::btor2file_parser::parse_btor2_file;
use unicorn::unicorn::builder::generate_model;
use unicorn::unicorn::codegen::compile_model_into_program;
use unicorn::unicorn::dimacs_parser::load_dimacs_as_gatemodel;
use unicorn::unicorn::emulate_loader::load_model_into_emulator;
use unicorn::unicorn::memory::replace_memory;
use unicorn::unicorn::optimize::{optimize_model, optimize_model_with_input};
use unicorn::unicorn::qubot::{InputEvaluator, Qubot};
use unicorn::unicorn::solver::*;

#[cfg(feature = "boolector")]
use unicorn::unicorn::boolector_impl;
use unicorn::unicorn::unroller::{prune_model, renumber_model, unroll_model};
use unicorn::unicorn::write_model;
#[cfg(feature = "z3")]
use unicorn::unicorn::z3solver_impl;

use ::unicorn::disassemble::disassemble;
use ::unicorn::emulate::EmulatorState;
use anyhow::{Context, Result};
use bytesize::ByteSize;
use cli::{collect_arg_values, expect_arg, expect_optional_arg, LogLevel, SmtType};
use env_logger::{Env, TimestampPrecision};
use riscu::load_object_file;
use std::{
    env,
    fs::File,
    io::{stdout, Write},
    path::PathBuf,
    str::FromStr,
};
use unicorn::unicorn::quarc::{evaluate_input, QuantumCircuit};

fn main() -> Result<()> {
    let matches = cli::args().get_matches();

    // process global flags
    let log_level = expect_arg::<LogLevel>(&matches, "verbose")?;

    init_logger(log_level)?;

    // process subcommands
    match matches.subcommand() {
        Some(("disassemble", args)) => {
            let input = expect_arg::<PathBuf>(args, "input-file")?;

            disassemble(input)
        }
        Some(("emulate", args)) => {
            let input = expect_arg::<PathBuf>(args, "input-file")?;
            let memory_size = ByteSize::mib(*args.get_one("memory").unwrap()).as_u64();
            let arg0 = expect_arg::<String>(args, "input-file")?;
            let extras = collect_arg_values(args, "extras");

            let argv = [vec![arg0], extras].concat();
            let program = load_object_file(&input)?;
            let mut emulator = EmulatorState::new(memory_size as usize);
            emulator.bootstrap(&program, &argv);
            emulator.run();

            Ok(())
        }
        Some(("beator", args)) | Some(("qubot", args)) | Some(("quarc", args)) => {
            let is_beator = matches.subcommand().unwrap().0 == "beator";
            let is_quarc = matches.subcommand().unwrap().0 == "quarc";
            let input = expect_arg::<PathBuf>(args, "input-file")?;
            let output = expect_optional_arg::<PathBuf>(args, "output-file")?;
            let unroll = args.get_one::<usize>("unroll-model").cloned();
            let solver = expect_arg::<SmtType>(args, "solver")?;
            let max_heap = *args.get_one::<u32>("max-heap").unwrap();
            let max_stack = *args.get_one::<u32>("max-stack").unwrap();
            let memory_size = ByteSize::mib(*args.get_one("memory").unwrap()).as_u64();
            let has_concrete_inputs = is_beator && args.contains_id("inputs");
            let inputs = expect_optional_arg::<String>(args, "inputs")?;
            let input_is_btor2 = args.contains_id("from-btor2");
            let prune = !is_beator || args.contains_id("prune-model");
            let input_is_dimacs = !is_beator && args.contains_id("from-dimacs");
            let compile_model = is_beator && args.contains_id("compile");
            let emulate_model = is_beator && args.contains_id("emulate");
            let arg0 = expect_arg::<String>(args, "input-file")?;
            let extras = collect_arg_values(args, "extras");
            println!("arg0: {:?}", arg0);

            let model = if !input_is_dimacs {
                let mut model = if !input_is_btor2 {
                    let program = load_object_file(&input)?;
                    let argv = [vec![arg0], extras].concat();
                    generate_model(&program, memory_size, max_heap, max_stack, &argv)?
                } else {
                    parse_btor2_file(&input)
                };

                if let Some(unroll_depth) = unroll {
                    model.lines.clear();
                    // TODO: Check if memory discretization is requested.
                    // TODO: Make emulate-loader work with discretized memory.
                    if !emulate_model && !compile_model {
                        replace_memory(&mut model);
                    }
                    let mut input_values: Vec<u64> = if has_concrete_inputs {
                        inputs
                            .as_ref()
                            .unwrap()
                            .split(',')
                            .map(|x| u64::from_str(x).unwrap())
                            .collect()
                    } else {
                        vec![]
                    };
                    for n in 0..unroll_depth {
                        unroll_model(&mut model, n);
                        if has_concrete_inputs {
                            optimize_model_with_input::<none_impl::NoneSolver>(
                                &mut model,
                                &mut input_values,
                            )
                        }
                    }
                    if prune {
                        prune_model(&mut model);
                    }
                    match solver {
                        SmtType::Generic => optimize_model::<none_impl::NoneSolver>(&mut model),
                        #[cfg(feature = "boolector")]
                        SmtType::Boolector => {
                            optimize_model::<boolector_impl::BoolectorSolver>(&mut model)
                        }
                        #[cfg(feature = "z3")]
                        SmtType::Z3 => optimize_model::<z3solver_impl::Z3SolverWrapper>(&mut model),
                    }
                    renumber_model(&mut model);
                }

                Some(model)
            } else {
                None
            };

            if compile_model {
                assert!(!input_is_btor2, "cannot compile arbitrary BTOR2");
                assert!(!input_is_dimacs, "cannot compile arbitrary DIMACS");

                // TODO: Just a workaround to get `argv` again.
                let arg0 = expect_arg::<String>(args, "input-file")?;
                let extras = collect_arg_values(args, "extras");
                let argv = [vec![arg0], extras].concat();

                let program = load_object_file(&input)?;
                let mut emulator = EmulatorState::new(memory_size as usize);
                // TODO: Eventually patch original program first, then bootstrap.
                emulator.bootstrap(&program, &argv); // bootstrap original program
                compile_model_into_program(&mut emulator, &model.unwrap(), &program);
                emulator.run();
                return Ok(());
            }

            if emulate_model {
                assert!(!input_is_btor2, "cannot emulate arbitrary BTOR2");
                assert!(!input_is_dimacs, "cannot emulate arbitrary DIMACS");

                let program = load_object_file(&input)?;
                let mut emulator = EmulatorState::new(memory_size as usize);
                emulator.prepare(&program); // only loads the code
                load_model_into_emulator(&mut emulator, &model.unwrap());
                emulator.run();
                return Ok(());
            }

            if is_beator {
                let bitblast = args.contains_id("bitblast");
                let dimacs = args.contains_id("dimacs");

                if bitblast {
                    let gate_model = bitblast_model(&model.unwrap(), true, 64);
                    if let Some(ref output_path) = output {
                        let file = File::create(output_path)?;
                        if dimacs {
                            write_dimacs_model(&gate_model, file)?;
                        } else {
                            write_btor2_model(&gate_model, file)?;
                        }
                    } else if dimacs {
                        write_dimacs_model(&gate_model, stdout())?;
                    } else {
                        write_btor2_model(&gate_model, stdout())?;
                    }
                } else if let Some(ref output_path) = output {
                    let file = File::create(output_path)?;
                    write_model(&model.unwrap(), file)?;
                } else {
                    write_model(&model.unwrap(), stdout())?;
                }
            } else if is_quarc {
                let m = model.unwrap();
                let mut qc = QuantumCircuit::new(&m, 64); // 64 is a paramater describing wordsize
                                                          // TODO: make wordsize parameter customizable from command line
                let _ = qc.process_model(1);
                if has_concrete_inputs {
                    let inputs = expect_optional_arg::<String>(args, "inputs")?;
                    let total_variables = qc.input_qubits.len();

                    if let Some(all_inputs) = inputs {
                        let instances: Vec<&str> = all_inputs.split('-').collect();

                        for instance in instances {
                            let mut values: Vec<i64> = instance
                                .split(',')
                                .map(|x| i64::from_str(x).unwrap())
                                .collect();
                            while values.len() < total_variables {
                                values.push(0);
                            }
                            println!(
                                "{}\n",
                                evaluate_input(
                                    &values,
                                    &qc.output_oracle,
                                    &qc.input_qubits,
                                    &qc.circuit_stack,
                                    &qc.dependencies
                                )
                                .0
                            );
                        }
                    } else {
                        panic!("This part of the code should be unreachable.");
                    }
                }
            } else {
                let is_ising = args.contains_id("ising");

                let gate_model = if !input_is_dimacs {
                    bitblast_model(&model.unwrap(), true, 64)
                } else {
                    load_dimacs_as_gatemodel(&input)?
                };

                let mut qubot = Qubot::new(&gate_model, is_ising);
                let bad_state_qubits = qubot.build_qubo();
                if let Some(ref output_path) = output {
                    let file = File::create(output_path)?;
                    qubot.dump_model(file, bad_state_qubits.clone())?;
                }
                qubot.dump_statistics();

                if let Some(all_inputs) = inputs {
                    let total_variables = gate_model.input_gates.len();
                    let instances: Vec<&str> = all_inputs.split('-').collect();

                    for instance in instances {
                        let mut values: Vec<i64> = instance
                            .split(',')
                            .map(|x| i64::from_str(x).unwrap())
                            .collect();

                        while values.len() < total_variables {
                            values.push(0);
                        }

                        let mut input_evaluator = InputEvaluator::new();
                        let (final_offset, true_bad_states) = input_evaluator.evaluate_inputs(
                            &qubot.qubo,
                            &qubot.mapping,
                            &gate_model.input_gates,
                            &values,
                            bad_state_qubits.clone(),
                        );
                        println!(
                            "offset:{}, bad states count:{}",
                            final_offset,
                            true_bad_states.len()
                        );
                    }
                }
            }

            Ok(())
        }

        Some(("dwave", args)) => {
            let input = args.get_one::<String>("input-file").unwrap();
            let runs = *args.get_one::<u32>("num-runs").unwrap();
            let chain_strength = *args.get_one::<f32>("chain-strength").unwrap();

            sample_quantum_annealer(input, runs, chain_strength)
        }
        _ => unreachable!(),
    }
}

fn init_logger(cli_log_level: LogLevel) -> Result<()> {
    let log_level_env_var = "MONSTER_LOG";
    let log_style_env_var = "MONSTER_LOG_STYLE";

    let env = Env::new()
        .filter_or::<&'static str, &'static str>(log_level_env_var, (&cli_log_level).into())
        .write_style_or(log_style_env_var, "always");

    let mut builder = env_logger::Builder::from_env(env);

    builder.format_timestamp(Some(TimestampPrecision::Millis));

    let level = env::var(log_style_env_var)
        .map_err(|e| e.to_string())
        .and_then(|s| LogLevel::from_str(s.as_str()).map_err(|e| e.to_string()))
        .unwrap_or(cli_log_level);

    if level == LogLevel::Info {
        builder.format(|buf, record| writeln!(buf, "{}", record.args()));
    }

    builder.try_init().context("Failed to initialize logger")
}
