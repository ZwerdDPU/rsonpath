use simdpath_core::bytes::*;
use simdpath_core::engine::result::*;
use simdpath_core::engine::runner::*;
use simdpath_core::query::*;

pub struct StacklessRunner<'a> {
    labels: Vec<&'a [u8]>,
}

impl<'a> Runner for StacklessRunner<'a> {
    fn count(&self, input: &str) -> CountResult {
        let count = automata::dispatch_automaton(&self.labels, input.as_bytes());

        CountResult { count }
    }
}

pub fn compile_query<'a>(query: &JsonPathQuery<'a>) -> StacklessRunner<'a> {
    let labels = query_to_descendant_pattern_labels(query);

    StacklessRunner { labels }
}

fn query_to_descendant_pattern_labels<'a>(query: &JsonPathQuery<'a>) -> Vec<&'a [u8]> {
    debug_assert!(query.root().is_root());
    let mut node_opt = query.root().child();
    let mut result = vec![];

    while let Some(node) = node_opt {
        match node {
            JsonPathQueryNode::Descendant(label_node) => match label_node.as_ref() {
                JsonPathQueryNode::Label(label, next_node) => {
                    result.push(*label);
                    node_opt = next_node.as_deref();
                }
                _ => panic! {"Unexpected type of node, expected Label."},
            },
            _ => panic! {"Unexpected type of node, expected Descendant."},
        }
    }

    result
}

mod automata {
    use simdpath_stackless_macros::*;

    initialize!();

    pub fn dispatch_automaton<'a>(labels: &Vec<&'a [u8]>, bytes: &'a [u8]) -> usize {
        dispatch_automaton!(labels, bytes)
    }
}
