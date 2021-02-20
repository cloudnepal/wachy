use crate::error::Error;
use crate::program;
use crate::program::{FunctionName, Program};
use crate::trace_structs::{CallInstruction, FrameInfo, InstructionType, TraceStack};
use crate::tracer::{TraceData, Tracer};
use crate::views;
use crate::views::TraceState;
use cursive::traits::{Nameable, Resizable};
use cursive::Cursive;
use program::SymbolInfo;
use std::borrow::Cow;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::io::BufRead;
use std::sync::{mpsc, Arc};
use zydis::enums::generated::{Mnemonic, Register};

pub struct Controller {
    program: Program,
    tracer: Tracer,
    trace_stack: Arc<TraceStack>,
}

impl Controller {
    pub fn run(program: Program, function_name: &str) -> Result<(), Error> {
        let matches = program.get_matches(function_name);
        // TODO ensure one and only one match
        let function = matches.into_iter().next().unwrap();

        let mut sview = views::new_source_view();
        let frame_info = Controller::setup_function(&program, function, &mut sview)?;

        let (stack_tx, stack_rx) = mpsc::channel();
        let trace_stack = Arc::new(TraceStack::new(
            program.file_path.clone(),
            frame_info,
            stack_tx,
        ));
        let (trace_tx, trace_rx) = mpsc::channel();
        let tracer = Tracer::new(Arc::clone(&trace_stack), trace_tx)?;

        let mut siv = cursive::default();
        siv.add_layer(
            cursive::views::Dialog::around(sview.with_name("source_view"))
                .title(format!("wachy | {}", program.file_path))
                .full_screen(),
        );
        Controller::add_callbacks(&mut siv);

        let controller = Controller {
            program,
            tracer,
            trace_stack,
        };
        siv.set_user_data(controller);

        siv.refresh();
        while siv.is_running() {
            siv.step();

            match stack_rx.try_recv() {
                Ok(_) => {
                    siv.user_data::<Controller>().unwrap().tracer.rerun_tracer();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(format!("Unexpected error: trace channel disconnected").into())
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }

            match trace_rx.try_recv() {
                Ok(data) => Controller::handle_trace_data(&mut siv, data)?,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(format!("Unexpected error: trace channel disconnected").into())
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
        Ok(())
    }

    fn handle_trace_data(siv: &mut Cursive, data: TraceData) -> Result<(), Error> {
        match data {
            TraceData::FatalError(message) => {
                siv.quit();
                Err(message.into())
            }
            TraceData::Data(data) => {
                // Ignore any data that doesn't correspond to current view. The trace command should
                // already be in the process of being updated.
                if !siv
                    .user_data::<Controller>()
                    .unwrap()
                    .trace_stack
                    .is_counter_current(data.counter)
                {
                    return Ok(());
                }
                siv.call_on_name("source_view", |sview: &mut views::SourceView| {
                    for (line, info) in data.traces {
                        let latency = if info.count != 0 {
                            TraceState::Traced(info.duration / u32::try_from(info.count).unwrap())
                        } else {
                            TraceState::Untraced
                        };
                        let frequency =
                            TraceState::Traced(info.count as f32 / data.time.as_secs_f32());
                        Self::set_line_state(sview, line, latency, frequency);
                    }
                });
                siv.refresh();
                Ok(())
            }
        }
    }

    fn setup_function(
        program: &Program,
        function: FunctionName,
        sview: &mut views::SourceView,
    ) -> Result<FrameInfo, Error> {
        let frame_info = Controller::create_frame_info(program, function)?;
        Controller::setup_source_view(&frame_info, sview)?;
        Ok(frame_info)
    }

    fn setup_source_view(
        frame_info: &FrameInfo,
        sview: &mut views::SourceView,
    ) -> Result<(), Error> {
        let source_code: Vec<String> = match std::fs::File::open(frame_info.get_source_file()) {
            Ok(file) => {
                // FIXME we can cache file contents
                std::io::BufReader::new(file)
                    .lines()
                    .map(|l| l.unwrap())
                    .collect()
            }
            Err(_) => {
                // TODO show error and confirm user wants to display empty lines
                // instead
                let max_line = frame_info.max_line();
                vec![String::new(); max_line as usize]
            }
        };
        views::set_source_view(
            sview,
            source_code,
            frame_info.get_source_line(),
            frame_info.called_lines(),
        );
        Ok(())
    }

    fn create_frame_info(program: &Program, function: FunctionName) -> Result<FrameInfo, Error> {
        let location = program.get_location(program.get_address(function)).ok_or_else(|| format!("Failed to get source information corresponding to function {}, please ensure {} has debugging symbols", function, program.file_path))?;
        let source_file = location.file.unwrap();
        let source_line = location.line.unwrap();
        log::info!(
            "Function {} is at {}:{}",
            function,
            source_file,
            source_line
        );

        let (start_address, code) = program.get_data(function).unwrap();
        let decoder = program::create_decoder();

        let mut line_to_callsites = HashMap::<u32, Vec<CallInstruction>>::new();

        for (instruction, ip) in
            program::get_instructions_with_mnemonic(&decoder, start_address, code, Mnemonic::CALL)
        {
            let relative_ip = u32::try_from(ip - start_address).unwrap();
            assert!(instruction.operand_count > 0);
            let operand = &instruction.operands[0];
            let call_instruction = match operand.reg {
                Register::NONE => match operand.mem.base {
                    Register::NONE => {
                        let call_address = instruction
                            .calc_absolute_address(ip, &instruction.operands[0])
                            .unwrap();
                        match program.get_function_for_address(call_address) {
                            Some(function) => {
                                if program.is_dynamic_symbol_address(call_address) {
                                    CallInstruction::dynamic_symbol(
                                        relative_ip,
                                        instruction.length,
                                        function,
                                    )
                                } else {
                                    CallInstruction::function(
                                        relative_ip,
                                        instruction.length,
                                        function,
                                    )
                                }
                            }
                            None => CallInstruction::unknown(relative_ip, instruction.length),
                        }
                    }
                    r => CallInstruction::register(
                        relative_ip,
                        instruction.length,
                        r.get_string().unwrap().to_string(),
                        Some(operand.mem.disp.displacement),
                    ),
                },
                r => {
                    // TODO convert register string to bpftrace register
                    CallInstruction::register(
                        relative_ip,
                        instruction.length,
                        r.get_string().unwrap().to_string(),
                        None,
                    )
                }
            };
            let location = program.get_location(ip).unwrap();
            if location.file.unwrap() == source_file {
                line_to_callsites
                    .entry(location.line.unwrap())
                    .or_default()
                    .push(call_instruction);
            } else {
                // This is an inlined call. We don't know which line it
                // corresponds to in the source file we are displaying, so we
                // have to drop it.
                log::trace!(
                    "Ignoring function call from {}:{} because it is not in current source file {}",
                    location.file.unwrap(),
                    location.line.unwrap(),
                    source_file
                );
            }
        }

        log::trace!("{:?}", line_to_callsites);
        let frame_info = FrameInfo::new(
            function,
            String::from(source_file),
            source_line,
            line_to_callsites,
        );

        Ok(frame_info)
    }

    fn set_line_state(
        sview: &mut views::SourceView,
        line: u32,
        latency: TraceState<std::time::Duration>,
        frequency: TraceState<f32>,
    ) {
        let item = sview.borrow_items_mut().get_mut(line as usize - 1).unwrap();
        item.latency = latency;
        item.frequency = frequency;
    }

    fn add_callbacks(siv: &mut Cursive) {
        siv.add_global_callback('x', |siv| {
            let mut sview = siv.find_name::<views::SourceView>("source_view").unwrap();
            let line = sview.row().unwrap() as u32 + 1;
            let trace_stack = &siv.user_data::<Controller>().unwrap().trace_stack;
            // We want to toggle tracing at this line - try to remove if it
            // exists, otherwise proceed to add callsite.
            if trace_stack.remove_callsite(line) {
                Self::set_line_state(
                    &mut *sview,
                    line,
                    TraceState::Untraced,
                    TraceState::Untraced,
                );
                return;
            }

            let callsites = trace_stack.get_callsites(line);
            if callsites.is_empty() {
                let function = trace_stack.get_current_function();
                siv.add_layer(views::new_dialog(&format!(
                    "No calls found in {} on line {}. Note the call may have been inlined.",
                    function, line
                )));
                return;
            }
            if callsites.len() > 1 {
                let search_view = views::new_search_view(
                    siv,
                    "Select the call to trace",
                    move |_siv, search, n_results| {
                        views::rank_fn(callsites.iter(), search, n_results)
                    },
                    move |siv: &mut Cursive, ci: &CallInstruction| {
                        let mut sview = siv.find_name::<views::SourceView>("source_view").unwrap();
                        Self::set_line_state(
                            &mut *sview,
                            line,
                            TraceState::Pending,
                            TraceState::Pending,
                        );
                        let controller = siv.user_data::<Controller>().unwrap();
                        controller.trace_stack.add_callsite(line, ci.clone());
                    },
                );
                siv.add_layer(search_view);
            } else {
                Self::set_line_state(&mut *sview, line, TraceState::Pending, TraceState::Pending);
                trace_stack.add_callsite(line, callsites.into_iter().nth(0).unwrap());
            }
        });

        siv.add_global_callback(
            cursive::event::Event::Key(cursive::event::Key::Enter),
            |siv| {
                let mut sview = siv.find_name::<views::SourceView>("source_view").unwrap();
                let line = sview.row().unwrap() as u32 + 1;
                let controller = &siv.user_data::<Controller>().unwrap();
                let trace_stack = &controller.trace_stack;
                let callsites = trace_stack.get_callsites(line);
                if callsites.is_empty() {
                    let function = trace_stack.get_current_function();
                    siv.add_layer(views::new_dialog(&format!(
                        "No calls found in {} on line {}. Note the call may have been inlined.",
                        function, line
                    )));
                    return;
                }

                let num_callsites = callsites.len();
                let direct_calls: Vec<SymbolInfo> = callsites
                    .into_iter()
                    .filter_map(|ci| match ci.instruction {
                        InstructionType::Unknown => None,
                        InstructionType::Register(_, _) => None,
                        InstructionType::DynamicSymbol(function) => {
                            Some(controller.program.get_symbol(function))
                        }
                        InstructionType::Function(function) => {
                            Some(controller.program.get_symbol(function))
                        }
                    })
                    .map(|si| si.clone())
                    .collect();
                // TODO allow entering any fn if dynamic call
                let num_indirect_calls = num_callsites - direct_calls.len();

                if num_callsites > 1 || num_indirect_calls > 0 {
                    // TODO we should be searching functions not callsites
                    let search_view = views::new_search_view(
                        siv,
                        "Select the call to enter",
                        move |siv: &mut Cursive, search: &str, mut n_results: usize| {
                            if search.is_empty() {
                                if num_indirect_calls > 0 {
                                    n_results -= 1;
                                }
                                let mut results =
                                    views::rank_fn(direct_calls.iter(), search, n_results);
                                if num_indirect_calls > 0 {
                                    let call_string = if num_indirect_calls == 1 {
                                        "1 indirect call".to_string()
                                    } else {
                                        format!("{} indirect calls", num_indirect_calls)
                                    };
                                    results
                                        .push((format!("{} (type to search)", call_string), None));
                                }
                                results
                            } else {
                                let controller = &siv.user_data::<Controller>().unwrap();
                                let mut it: Box<dyn Iterator<Item = &SymbolInfo>> =
                                    Box::new(direct_calls.iter());
                                if num_indirect_calls > 0 {
                                    it = Box::new(it.chain(controller.program.symbols_iterator()));
                                }
                                views::rank_fn(it, search, n_results)
                            }
                        },
                        move |siv: &mut Cursive, symbol: &SymbolInfo| {
                            let controller = &siv.user_data::<Controller>().unwrap();
                            if controller.program.is_dynamic_symbol(symbol) {
                                // TODO show error for dyn fn
                            } else {
                                let mut sview =
                                    siv.find_name::<views::SourceView>("source_view").unwrap();
                                let controller = siv.user_data::<Controller>().unwrap();
                                // TODO don't expect
                                let frame_info = Controller::setup_function(
                                    &controller.program,
                                    symbol.name,
                                    &mut *sview,
                                )
                                .expect(&format!("Error setting up function {}", symbol.name));
                                controller.trace_stack.push(frame_info);
                            }
                            // TODO show error for dyn fn
                        },
                    );
                    siv.add_layer(search_view);
                } else {
                    let symbol = &direct_calls[0];
                    if controller.program.is_dynamic_symbol(symbol) {
                        // TODO show error for dyn fn
                    } else {
                        let frame_info = Controller::setup_function(
                            &controller.program,
                            symbol.name,
                            &mut *sview,
                        )
                        .expect(&format!("Error setting up function {}", symbol.name));
                        trace_stack.push(frame_info);
                    }
                }
            },
        );

        siv.add_global_callback(
            cursive::event::Event::Key(cursive::event::Key::Esc),
            |siv| {
                if siv.screen().len() > 1 {
                    // Pop anything on top of source view
                    siv.pop_layer();
                    return;
                }
                let controller = &siv.user_data::<Controller>().unwrap();
                match controller.trace_stack.pop() {
                    Some(frame_info) => {
                        let mut sview = siv.find_name::<views::SourceView>("source_view").unwrap();
                        Controller::setup_source_view(&frame_info, &mut *sview).unwrap();
                    }
                    None => siv.add_layer(views::new_quit_dialog("Are you sure you want to quit?")),
                }
            },
        );
    }
}

impl views::Label for CallInstruction {
    fn label(&self) -> Cow<str> {
        Cow::Owned(self.to_string())
    }
}
impl views::Label for program::SymbolInfo {
    fn label(&self) -> Cow<str> {
        Cow::Borrowed(self.as_ref())
    }
}
