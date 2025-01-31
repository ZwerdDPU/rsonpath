//! Main implementation of a JSONPath query engine.
//!
//! Core engine for processing of JSONPath queries, based on the
//! [Stackless Processing of Streamed Trees](https://hal.archives-ouvertes.fr/hal-03021960) paper.
//! Entire query execution is done without recursion or an explicit stack, linearly through
//! the JSON structure, which allows efficient SIMD operations and optimized register usage.
//!
//! This implementation should be more performant than [`recursive`](super::recursive::RecursiveEngine)
//! even on targets that do not support AVX2 SIMD operations.
#[cfg(feature = "head-skip")]
use super::head_skipping::{CanHeadSkip, HeadSkip};
use super::Compiler;
#[cfg(feature = "unique-labels")]
use crate::classification::structural::BracketType;
#[cfg(feature = "head-skip")]
use crate::classification::ResumeClassifierState;
use crate::classification::{
    quotes::{classify_quoted_sequences, QuoteClassifiedIterator},
    structural::{classify_structural_characters, Structural, StructuralIterator},
};
use crate::debug;
use crate::engine::depth::Depth;
use crate::engine::error::EngineError;
#[cfg(feature = "tail-skip")]
use crate::engine::tail_skipping::TailSkip;
use crate::engine::{Engine, Input};
use crate::query::automaton::{Automaton, State};
use crate::query::error::CompilerError;
use crate::query::{JsonPathQuery, Label};
use crate::result::QueryResult;
use aligners::{alignment, AlignedBytes};
use smallvec::{smallvec, SmallVec};

/// Main engine for a fixed JSONPath query.
///
/// The engine is stateless, meaning that it can be executed
/// on any number of separate inputs, even on separate threads.
pub struct MainEngine<'q> {
    automaton: Automaton<'q>,
}

impl Compiler for MainEngine<'_> {
    type E<'q> = MainEngine<'q>;

    #[must_use = "compiling the query only creates an engine instance that should be used"]
    #[inline(always)]
    fn compile_query(query: &JsonPathQuery) -> Result<MainEngine, CompilerError> {
        let automaton = Automaton::new(query)?;
        debug!("DFA:\n {}", automaton);
        Ok(MainEngine { automaton })
    }

    #[inline(always)]
    fn from_compiled_query(automaton: Automaton<'_>) -> Self::E<'_> {
        MainEngine { automaton }
    }
}

impl Engine for MainEngine<'_> {
    #[inline]
    fn run<R: QueryResult>(&self, input: &Input) -> Result<R, EngineError> {
        if self.automaton.is_empty_query() {
            return Ok(empty_query(input));
        }

        let mut result = R::default();
        let executor = query_executor(&self.automaton, input);
        executor.run(&mut result)?;

        Ok(result)
    }
}

fn empty_query<R: QueryResult>(bytes: &AlignedBytes<alignment::Page>) -> R {
    let quote_classifier = classify_quoted_sequences(bytes.relax_alignment());
    let mut block_event_source = classify_structural_characters(quote_classifier);
    let mut result = R::default();

    if let Some(Structural::Opening(_, idx)) = block_event_source.next() {
        result.report(idx);
    }

    result
}

#[cfg(feature = "tail-skip")]
macro_rules! Classifier {
    () => {
        TailSkip<'b, Q, I>
    };
}
#[cfg(not(feature = "tail-skip"))]
macro_rules! Classifier {
    () => {
        I
    };
}

struct Executor<'q, 'b> {
    depth: Depth,
    state: State,
    stack: SmallStack,
    automaton: &'b Automaton<'q>,
    bytes: &'b AlignedBytes<alignment::Page>,
    next_event: Option<Structural>,
    is_list: bool,
}

fn query_executor<'q, 'b>(
    automaton: &'b Automaton<'q>,
    bytes: &'b AlignedBytes<alignment::Page>,
) -> Executor<'q, 'b> {
    Executor {
        depth: Depth::ZERO,
        state: automaton.initial_state(),
        stack: SmallStack::new(),
        automaton,
        bytes,
        next_event: None,
        is_list: false,
    }
}

impl<'q, 'b> Executor<'q, 'b> {
    #[cfg(feature = "head-skip")]
    fn run<R: QueryResult>(mut self, result: &mut R) -> Result<(), EngineError> {
        let mb_head_skip = HeadSkip::new(self.bytes, self.automaton);

        match mb_head_skip {
            Some(head_skip) => head_skip.run_head_skipping(&mut self, result),
            None => self.run_and_exit(result),
        }
    }

    #[cfg(not(feature = "head-skip"))]
    fn run<R: QueryResult>(self, result: &mut R) -> Result<(), EngineError> {
        self.run_and_exit(result)
    }

    fn run_and_exit<R: QueryResult>(mut self, result: &mut R) -> Result<(), EngineError> {
        let quote_classifier = classify_quoted_sequences(self.bytes.relax_alignment());
        let structural_classifier = classify_structural_characters(quote_classifier);
        #[cfg(feature = "tail-skip")]
        let mut classifier = TailSkip::new(structural_classifier);
        #[cfg(not(feature = "tail-skip"))]
        let mut classifier = structural_classifier;

        self.run_on_subtree(&mut classifier, result)?;

        self.verify_subtree_closed()
    }

    fn run_on_subtree<
        Q: QuoteClassifiedIterator<'b>,
        I: StructuralIterator<'b, Q>,
        R: QueryResult,
    >(
        &mut self,
        classifier: &mut Classifier!(),
        result: &mut R,
    ) -> Result<(), EngineError> {
        while let Some(event) = self.next_event.or_else(|| classifier.next()) {
            debug!("====================");
            debug!("Event = {:?}", event);
            debug!("Depth = {:?}", self.depth);
            debug!("Stack = {:?}", self.stack);
            debug!("State = {:?}", self.state);
            debug!("====================");

            self.next_event = None;
            match event {
                Structural::Colon(idx) => self.handle_colon(classifier, idx, result)?,
                Structural::Comma(idx) => self.handle_comma(classifier, idx, result)?,
                Structural::Opening(_, idx) => self.handle_opening(classifier, idx, result)?,
                Structural::Closing(_, idx) => {
                    self.handle_closing(classifier, idx)?;

                    if self.depth == Depth::ZERO {
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    fn handle_colon<Q, I, R>(
        &mut self,
        classifier: &mut Classifier!(),
        idx: usize,
        result: &mut R,
    ) -> Result<(), EngineError>
    where
        Q: QuoteClassifiedIterator<'b>,
        I: StructuralIterator<'b, Q>,
        R: QueryResult,
    {
        debug!(
            "Colon, label ending with {:?}",
            std::str::from_utf8(&self.bytes[(if idx < 8 { 0 } else { idx - 8 })..idx])
                .unwrap_or("[invalid utf8]")
        );

        self.next_event = classifier.next();
        let is_next_opening = self.next_event.map_or(false, |s| s.is_opening());

        if !is_next_opening {
            let mut any_matched = false;

            for &(label, target) in self.automaton[self.state].transitions() {
                if self.automaton.is_accepting(target) && self.is_match(idx, label)? {
                    result.report(idx);
                    any_matched = true;
                    break;
                }
            }
            let fallback_state = self.automaton[self.state].fallback_state();
            if !any_matched && self.automaton.is_accepting(fallback_state) {
                result.report(idx);
            }
            #[cfg(feature = "unique-labels")]
            {
                let is_next_closing = self.next_event.map_or(false, |s| s.is_closing());
                if any_matched && !is_next_closing && self.automaton.is_unitary(self.state) {
                    let opening = if self.is_list { b'[' } else { b'{' };
                    debug!("Skipping unique state from {}", opening as char);
                    let stop_at = classifier.skip(opening);
                    let bracket_type = if self.is_list {
                        BracketType::Square
                    } else {
                        BracketType::Curly
                    };
                    self.next_event = Some(Structural::Closing(bracket_type, stop_at));
                }
            }
        }

        Ok(())
    }

    fn handle_comma<Q, I, R>(
        &mut self,
        classifier: &mut Classifier!(),
        idx: usize,
        result: &mut R,
    ) -> Result<(), EngineError>
    where
        Q: QuoteClassifiedIterator<'b>,
        I: StructuralIterator<'b, Q>,
        R: QueryResult,
    {
        self.next_event = classifier.next();
        let is_next_opening = self.next_event.map_or(false, |s| s.is_opening());

        if !is_next_opening {
            let fallback_state = self.automaton[self.state].fallback_state();
            if self.is_list && self.automaton.is_accepting(fallback_state) {
                result.report(idx);
            }
        }

        Ok(())
    }

    fn handle_opening<Q, I, R>(
        &mut self,
        classifier: &mut Classifier!(),
        idx: usize,
        result: &mut R,
    ) -> Result<(), EngineError>
    where
        Q: QuoteClassifiedIterator<'b>,
        I: StructuralIterator<'b, Q>,
        R: QueryResult,
    {
        debug!(
            "Opening {}, increasing depth and pushing stack.",
            self.bytes[idx]
        );
        let mut any_matched = false;

        if let Some(colon_idx) = self.find_preceding_colon(idx) {
            debug!(
                "Colon backtracked, label ending with {:?}",
                std::str::from_utf8(
                    &self.bytes[(if colon_idx < 8 { 0 } else { colon_idx - 8 })..colon_idx]
                )
                .unwrap_or("[invalid utf8]")
            );
            for &(label, target) in self.automaton[self.state].transitions() {
                if self.is_match(colon_idx, label)? {
                    any_matched = true;
                    self.transition_to(target, self.bytes[idx]);
                    if self.automaton.is_accepting(target) {
                        result.report(colon_idx);
                    }
                    break;
                }
            }
        }

        if !any_matched && self.depth != Depth::ZERO {
            let fallback = self.automaton[self.state].fallback_state();
            debug!("Falling back to {fallback}");

            #[cfg(feature = "tail-skip")]
            if self.automaton.is_rejecting(fallback) {
                classifier.skip(self.bytes[idx]);
                return Ok(());
            } else {
                self.transition_to(fallback, self.bytes[idx]);
            }
            #[cfg(not(feature = "tail-skip"))]
            self.transition_to(fallback, self.bytes[idx]);

            if self.automaton.is_accepting(fallback) {
                result.report(idx);
            }
        }

        if self.bytes[idx] == b'[' {
            self.is_list = true;

            let fallback = self.automaton[self.state].fallback_state();
            if self.automaton.is_accepting(fallback) {
                classifier.turn_commas_on(idx);
                self.next_event = classifier.next();
                match self.next_event {
                    Some(Structural::Closing(_, close_idx)) => {
                        for next_idx in (idx + 1)..close_idx {
                            if !self.bytes[next_idx].is_ascii_whitespace() {
                                result.report(next_idx);
                                break;
                            }
                        }
                    }
                    Some(Structural::Comma(_)) => {
                        result.report(idx + 1);
                    }
                    _ => (),
                }
            } else {
                classifier.turn_commas_off();
            }
        } else {
            self.is_list = false;
        }

        if !self.is_list && self.automaton.has_transition_to_accepting(self.state) {
            classifier.turn_colons_on(idx);
        } else {
            classifier.turn_colons_off();
        }
        self.depth
            .increment()
            .map_err(|err| EngineError::DepthAboveLimit(idx, err))?;

        Ok(())
    }

    fn handle_closing<Q, I>(
        &mut self,
        classifier: &mut Classifier!(),
        idx: usize,
    ) -> Result<(), EngineError>
    where
        Q: QuoteClassifiedIterator<'b>,
        I: StructuralIterator<'b, Q>,
    {
        debug!("Closing, decreasing depth and popping stack.");

        #[cfg(feature = "unique-labels")]
        {
            self.depth
                .decrement()
                .map_err(|err| EngineError::DepthBelowZero(idx, err))?;

            if let Some(stack_frame) = self.stack.pop_if_at_or_below(*self.depth) {
                self.state = stack_frame.state;
                self.is_list = stack_frame.is_list;

                if self.automaton.is_unitary(self.state) {
                    let opening = if self.is_list { b'[' } else { b'{' };
                    debug!("Skipping unique state from {}", opening as char);
                    let close_idx = classifier.skip(opening);
                    let bracket_type = if self.is_list {
                        BracketType::Square
                    } else {
                        BracketType::Curly
                    };
                    self.next_event = Some(Structural::Closing(bracket_type, close_idx));
                    return Ok(());
                }
            }
        }
        #[cfg(not(feature = "unique-labels"))]
        {
            self.depth
                .decrement()
                .map_err(|err| EngineError::DepthBelowZero(idx, err))?;

            if let Some(stack_frame) = self.stack.pop_if_at_or_below(*self.depth) {
                self.state = stack_frame.state;
                self.is_list = stack_frame.is_list;
            }
        }

        if self.is_list
            && self
                .automaton
                .is_accepting(self.automaton[self.state].fallback_state())
        {
            classifier.turn_commas_on(idx);
        } else {
            classifier.turn_commas_off();
        }

        if !self.is_list && self.automaton.has_transition_to_accepting(self.state) {
            classifier.turn_colons_on(idx);
        } else {
            classifier.turn_colons_off();
        }

        Ok(())
    }

    fn transition_to(&mut self, target: State, opening: u8) {
        let target_is_list = opening == b'[';
        if target != self.state || target_is_list != self.is_list {
            debug!(
                "push {}, goto {target}, is_list = {target_is_list}",
                self.state
            );
            self.stack.push(StackFrame {
                depth: *self.depth,
                state: self.state,
                is_list: self.is_list,
            });
            self.state = target;
        }
    }

    fn find_preceding_colon(&self, idx: usize) -> Option<usize> {
        if self.depth == Depth::ZERO {
            None
        } else {
            let mut colon_idx = idx - 1;
            while self.bytes[colon_idx].is_ascii_whitespace() {
                colon_idx -= 1;
            }
            (self.bytes[colon_idx] == b':').then_some(colon_idx)
        }
    }

    fn is_match(&self, idx: usize, label: &Label) -> Result<bool, EngineError> {
        let len = label.len() + 2;

        let mut closing_quote_idx = idx - 1;
        while self.bytes[closing_quote_idx] != b'"' {
            if closing_quote_idx == 0 {
                return Err(EngineError::MalformedLabelQuotes(idx));
            }

            closing_quote_idx -= 1;
        }

        if closing_quote_idx + 1 < len {
            return Ok(false);
        }

        let start_idx = closing_quote_idx + 1 - len;
        let slice = &self.bytes[start_idx..closing_quote_idx + 1];

        Ok(label.bytes_with_quotes() == slice
            && (start_idx == 0 || self.bytes[start_idx - 1] != b'\\'))
    }

    fn verify_subtree_closed(&self) -> Result<(), EngineError> {
        if self.depth != Depth::ZERO {
            Err(EngineError::MissingClosingCharacter())
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct StackFrame {
    depth: u8,
    state: State,
    is_list: bool,
}

#[derive(Debug)]
struct SmallStack {
    contents: SmallVec<[StackFrame; 128]>,
}

impl SmallStack {
    fn new() -> Self {
        Self {
            contents: smallvec![],
        }
    }

    #[inline]
    fn peek(&mut self) -> Option<StackFrame> {
        self.contents.last().copied()
    }

    #[inline]
    fn pop_if_at_or_below(&mut self, depth: u8) -> Option<StackFrame> {
        if let Some(stack_frame) = self.peek() {
            if depth <= stack_frame.depth {
                return self.contents.pop();
            }
        }
        None
    }

    #[inline]
    fn push(&mut self, value: StackFrame) {
        self.contents.push(value)
    }
}

#[cfg(feature = "head-skip")]
impl<'q, 'b> CanHeadSkip<'b> for Executor<'q, 'b> {
    fn run_on_subtree<'r, R, Q, I>(
        &mut self,
        next_event: Structural,
        state: State,
        structural_classifier: I,
        result: &'r mut R,
    ) -> Result<ResumeClassifierState<'b, Q>, EngineError>
    where
        Q: QuoteClassifiedIterator<'b>,
        R: QueryResult,
        I: StructuralIterator<'b, Q>,
    {
        #[cfg(feature = "tail-skip")]
        let mut classifier = TailSkip::new(structural_classifier);
        #[cfg(not(feature = "tail-skip"))]
        let mut classifier = structural_classifier;

        self.state = state;
        self.next_event = Some(next_event);

        self.run_on_subtree(&mut classifier, result)?;
        self.verify_subtree_closed()?;

        Ok(classifier.stop())
    }
}
