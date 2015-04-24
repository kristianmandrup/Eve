use value::{Value, Tuple, Relation};
use index::{Index};
use query::{Ref, ConstraintOp, Constraint, Source, Clause, Query};
use interpreter::{EveFn,Pattern};
use interpreter;
use flow::{View, Union, Node, Flow};

use std::collections::{BitSet};
use std::cell::{RefCell};
use std::num::ToPrimitive;

impl Index<Tuple> {
    pub fn find_all(&self, ix: usize, value: &Value) -> Vec<&Tuple> {
        self.iter().filter(|t| &t[ix] == value).collect()
    }

    pub fn find_one(&self, ix: usize, value: &Value) -> &Tuple {
        match &*self.find_all(ix, value) {
            [] => panic!("No tuples with tuple[{}] = {:?}", ix, value),
            [t] => t,
            _ => panic!("Multiple tuples with tuple[{}] = {:?}", ix, value),
        }
    }
}

// TODO
// check schema, field, view
// check schemas on every view
// check every view has a schema
// check every non-empty view is a view with kind=input
// fill in missing views with empty indexes
// check upstream etc are empty
// gather function refs
// poison rows in rounds until changes stop
//   foreign keys don't exist or are poisoned
//   ixes are not 0-n

static COMPILER_VIEWS: [&'static str; 7] =
["view", "source", "constraint", "view-mapping", "field-mapping", "schedule", "upstream"];

static VIEW_ID: usize = 0;
static VIEW_SCHEMA: usize = 1;
static VIEW_KIND: usize = 2;

static FIELD_SCHEMA: usize = 0;
static FIELD_IX: usize = 1;
static FIELD_ID: usize = 2;

static SOURCE_VIEW: usize = 0;
static SOURCE_IX: usize = 1;
static SOURCE_ID: usize = 2;
static SOURCE_DATA: usize = 3;
static SOURCE_ACTION: usize = 4;

static CONSTRAINT_LEFT: usize = 0;
static CONSTRAINT_OP: usize = 1;
static CONSTRAINT_RIGHT: usize = 2;

static VIEWMAPPING_ID: usize = 0;
static VIEWMAPPING_SOURCEVIEW: usize = 1;
static VIEWMAPPING_SINKVIEW: usize = 2;

static FIELDMAPPING_VIEWMAPPING: usize = 0;
static FIELDMAPPING_SOURCEREF: usize = 1;
static FIELDMAPPING_SINKFIELD: usize = 2;

static CALL_FUN: usize = 1;
static CALL_ARGS: usize = 2;

static MATCH_INPUT: usize = 1;
static MATCH_PATTERNS: usize = 2;
static MATCH_HANDLES: usize = 3;

static COLUMN_SOURCE_ID: usize = 1;
static COLUMN_FIELD_ID: usize = 2;

static SCHEDULE_IX: usize = 0;
static SCHEDULE_VIEW: usize = 1;

static UPSTREAM_DOWNSTREAM: usize = 0;
static UPSTREAM_IX: usize = 1;
static UPSTREAM_UPSTREAM: usize = 2;

struct Compiler {
    flow: Flow,
    upstream: Relation,
    schedule: Relation,
    ordered_constraint: Relation,
}

fn create_upstream(flow: &Flow) -> Relation {
    let mut upstream = Index::new();
    for view in flow.get_state("view").iter() {
        let downstream_id = &view[VIEW_ID];
        let kind = &view[VIEW_KIND];
        let mut ix = 0.0;
        match kind.as_str() {
            "input" => (),
            "query" => {
                for source in flow.get_state("source").find_all(SOURCE_VIEW, downstream_id) {
                    let data = &source[SOURCE_DATA];
                    if data[0].as_str() == "view"  {
                        let upstream_id = &data[1];
                        upstream.insert(vec![
                            downstream_id.clone(),
                            Value::Float(ix),
                            upstream_id.clone(),
                            ]);
                        ix += 1.0;
                    }
                }
            }
            "union" => {
                for view_mapping in flow.get_state("view-mapping").find_all(VIEWMAPPING_SINKVIEW, downstream_id) {
                    let upstream_id = &view_mapping[VIEWMAPPING_SOURCEVIEW];
                        upstream.insert(vec![
                            downstream_id.clone(),
                            Value::Float(ix),
                            upstream_id.clone(),
                            ]);
                    ix += 1.0;
                }
            }
            other => panic!("Unknown view kind: {}", other)
        }
    }
    upstream
}

fn create_schedule(flow: &Flow) -> Relation {
    // TODO actually schedule sensibly
    // TODO warn about cycles through aggregates
    let mut schedule = Index::new();
    let mut ix = 0.0;
    for view in flow.get_state("view").iter() {
        let view_id = &view[VIEW_ID];
        schedule.insert(vec![Value::Float(ix), view_id.clone()]);
        ix += 1.0;
    }
    schedule
}

// hackily reorder constraints to match old assumptions in create_constraint
fn create_ordered_constraint(flow: &Flow) -> Relation {
    let mut ordered_constraint = Index::new();
    for constraint in flow.get_state("constraint").iter() {
        let left = constraint[CONSTRAINT_LEFT].clone();
        let op = constraint[CONSTRAINT_OP].clone();
        let right = constraint[CONSTRAINT_RIGHT].clone();
        assert!((left[0].as_str() != "constant") || (right[0].as_str() != "constant"));
        if get_ref_ix(flow, &left) >= get_ref_ix(flow, &right) {
            ordered_constraint.insert(vec![left, op, right]);
        } else {
            ordered_constraint.insert(vec![right, op, left]);
        }
    }
    ordered_constraint
}

fn get_view_ix(schedule: &Relation, view_id: &Value) -> usize {
    schedule.find_one(SCHEDULE_VIEW, view_id)[SCHEDULE_IX].to_usize().unwrap()
}

fn get_source_ix(flow: &Flow, source_id: &Value) -> usize {
    flow.get_state("source").find_one(SOURCE_ID, source_id)[SOURCE_IX].to_usize().unwrap()
}

fn get_field_ix(flow: &Flow, field_id: &Value) -> usize {
    flow.get_state("field").find_one(FIELD_ID, field_id)[FIELD_IX].to_usize().unwrap()
}

fn get_num_fields(flow: &Flow, view_id: &Value) -> usize {
    let schema_id = flow.get_state("view").find_one(VIEW_ID, view_id)[VIEW_SCHEMA].clone();
    flow.get_state("field").find_all(FIELD_SCHEMA, &schema_id).len()
}

fn get_ref_ix(flow: &Flow, reference: &Value) -> i64 {
    match reference[0].as_str() {
        "constant" => -1, // constants effectively are calculated before any sources
        "column" => get_source_ix(flow, &reference[1]) as i64,
        other => panic!("Unknown ref type: {:?}", other),
    }
}

fn create_reference(compiler: &Compiler, reference: &Value) -> Ref {
    match reference[0].as_str() {
        "constant" => {
            let value = reference[1].clone();
            Ref::Constant{
                value: value,
            }
        }
        "column" => {
            let other_source_id = &reference[COLUMN_SOURCE_ID];
            let other_field_id = &reference[COLUMN_FIELD_ID];
            let other_source_ix = get_source_ix(&compiler.flow, other_source_id);
            let other_field_ix = get_field_ix(&compiler.flow, other_field_id);
            Ref::Value{
                clause: other_source_ix,
                column: other_field_ix,
            }
        }
        other => panic!("Unknown ref kind: {}", other)
    }
}

fn create_constraint(compiler: &Compiler, constraint: &Vec<Value>) -> Constraint {
    let my_column = get_field_ix(&compiler.flow, &constraint[CONSTRAINT_LEFT][2]);
    let op = match constraint[CONSTRAINT_OP].as_str() {
        "<" => ConstraintOp::LT,
        "<=" => ConstraintOp::LTE,
        "=" => ConstraintOp::EQ,
        "!=" => ConstraintOp::NEQ,
        ">" => ConstraintOp::GT,
        ">=" => ConstraintOp::GTE,
        other => panic!("Unknown constraint op: {}", other),
    };
    let other_ref = create_reference(compiler, &constraint[CONSTRAINT_RIGHT]);
    Constraint{
        my_column: my_column,
        op: op,
        other_ref: other_ref,
    }
}

fn create_source(compiler: &Compiler, source: &Vec<Value>) -> Source {
    let source_id = &source[SOURCE_ID];
    let source_view_id = &source[SOURCE_VIEW];
    let source_data = &source[SOURCE_DATA];
    let other_view_id = &source_data[1];
    let upstream = compiler.upstream.iter().filter(|upstream| {
        (upstream[UPSTREAM_DOWNSTREAM] == *source_view_id) &&
        (upstream[UPSTREAM_UPSTREAM] == *other_view_id)
    }).next().unwrap();
    let other_view_ix = &upstream[UPSTREAM_IX];
    let constraints = compiler.ordered_constraint.iter().filter(|constraint| {
        constraint[CONSTRAINT_LEFT][1] == *source_id
    }).map(|constraint| {
        create_constraint(compiler, constraint)
    }).collect::<Vec<_>>();
    Source{
        relation: other_view_ix.to_usize().unwrap(),
        constraints: constraints,
    }
}

fn create_expression(compiler: &Compiler, expression: &Value) -> interpreter::Expression {
    match expression[0].as_str() {
        "call" => interpreter::Expression::Call(create_call(compiler,&expression[CALL_FUN],&expression[CALL_ARGS])),
        "match" => interpreter::Expression::Match(Box::new(create_match(compiler,&expression[MATCH_INPUT],&expression[MATCH_PATTERNS],&expression[MATCH_HANDLES]))),
        other => panic!("Unknown expression type: {:?}", other),
    }
}

fn create_clause(compiler: &Compiler, source: &Vec<Value>) -> Clause {
    let source_data = &source[SOURCE_DATA];
    match source_data[0].as_str() {
        "view" => {
            match source[SOURCE_ACTION].as_str() {
                "get-tuple" => Clause::Tuple(create_source(compiler, source)),
                "get-relation" => Clause::Relation(create_source(compiler, source)),
                other => panic!("Unknown view action: {}", other),
            }
        }
        "expression" => {
            Clause::Expression(create_expression(compiler, &source_data[1]))
        }
        other => panic!("Unknown clause type: {:?}", other)
    }
}


fn create_match(compiler: &Compiler, uiinput: &Value, uipatterns: &Value, uihandlers: &Value) -> interpreter::Match {

    // Create the input
    let match_input = create_call_arg(compiler,uiinput.as_slice());

    // Create the pattern vector
    let match_patterns = uipatterns.as_slice()
                        .iter()
                        .map(|arg| {
                            let call_arg = create_call_arg(compiler,arg.as_slice());
                            match call_arg {
                                interpreter::Expression::Ref(x) => Pattern::Constant(x),
                                _ => panic!("TODO"),
                                }
                            }
                        )
                        .collect();

    // Create handles vector
    let match_handlers = uihandlers.as_slice()
                            .iter()
                            .map(|arg| create_call_arg(compiler,arg.as_slice()))
                            .collect();

    // Compile the match
    interpreter::Match{input: match_input, patterns: match_patterns, handlers: match_handlers}
}

fn create_call(compiler: &Compiler, uifun: &Value, uiargvec: &Value) -> interpreter::Call {

    // Match the uifun with an EveFn...
    let evefn = match uifun.as_str() {
        "+"   => EveFn::Add,
        "-"   => EveFn::Subtract,
        "*"   => EveFn::Multiply,
        "/"   => EveFn::Divide,
        "sum" => EveFn::Sum,
        _     => panic!("Unknown Function Call: {:?}",uifun),
    };

    let args = uiargvec.as_slice()
                       .iter()
                       .map(|arg| create_call_arg(compiler, arg.as_slice()))
                       .collect();

    interpreter::Call{fun: evefn, args: args}
}

fn create_call_arg(compiler: &Compiler, arg: &[Value]) -> interpreter::Expression {

    match arg[0].as_str() {
        "constant" => {
            assert_eq!(arg.len(),2 as usize);
            interpreter::Expression::Ref(Ref::Constant{value: arg[1].clone()})
        },
        "column" => {
            assert_eq!(arg.len(),3 as usize);
            let other_source_id = &arg[COLUMN_SOURCE_ID];
            let other_field_id = &arg[COLUMN_FIELD_ID];
            let other_source_ix = get_source_ix(&compiler.flow, other_source_id);
            let other_field_ix = get_field_ix(&compiler.flow, other_field_id);

            interpreter::Expression::Ref(Ref::Value{ clause: other_source_ix, column: other_field_ix })
        },
        "call" => interpreter::Expression::Call(create_call(compiler,&arg[CALL_FUN],&arg[CALL_ARGS])),
        other  => panic!("Unhandled ref kind: {:?}", other),
    }
}

fn create_query(compiler: &Compiler, view_id: &Value) -> Query {
    // arrives in ix order
    let clauses = compiler.flow.get_state("source")
                       .find_all(SOURCE_VIEW, view_id)
                       .iter()
                       .map(|source| create_clause(compiler, source))
                       .collect();
    Query{clauses: clauses}
}

fn create_union(compiler: &Compiler, view_id: &Value) -> Union {
    let num_sink_fields = get_num_fields(&compiler.flow, view_id);
    let mut view_mappings = Vec::new();
    for upstream in compiler.upstream.find_all(UPSTREAM_DOWNSTREAM, view_id) { // arrives in ix order
        let source_view_id = &upstream[UPSTREAM_UPSTREAM];
        let view_mapping = compiler.flow.get_state("view-mapping").find_one(VIEWMAPPING_SOURCEVIEW, source_view_id).clone();
        let view_mapping_id = &view_mapping[VIEWMAPPING_ID];
        let mut field_mappings = vec![None; num_sink_fields];
        for field_mapping in compiler.flow.get_state("field-mapping").find_all(FIELDMAPPING_VIEWMAPPING, &view_mapping_id) {
            let source_ref = create_reference(compiler, &field_mapping[FIELDMAPPING_SOURCEREF]);
            let sink_field_id = &field_mapping[FIELDMAPPING_SINKFIELD];
            let sink_field_ix = get_field_ix(&compiler.flow, sink_field_id);
            field_mappings[sink_field_ix] = Some(source_ref);
        }
        let field_mappings = field_mappings.drain().map(|reference| reference.unwrap()).collect();
        let num_source_fields = get_num_fields(&compiler.flow, source_view_id);
        view_mappings.push((num_source_fields, field_mappings));
    }
    Union{mappings: view_mappings}
}

fn create_node(compiler: &Compiler, view_id: &Value, view_kind: &Value) -> Node {
    let view = match view_kind.as_str() {
        "input" => View::Input,
        "query" => View::Query(create_query(compiler, view_id)),
        "union" => View::Union(create_union(compiler, view_id)),
        other => panic!("Unknown view kind: {}", other)
    };
    let upstream = compiler.upstream.find_all(UPSTREAM_DOWNSTREAM, view_id).iter().map(|upstream| {
        get_view_ix(&compiler.schedule, &upstream[UPSTREAM_UPSTREAM])
    }).collect(); // arrives in ix order so it will match the arg order selected by create_query/union
    let downstream = compiler.upstream.find_all(UPSTREAM_UPSTREAM, view_id).iter().map(|upstream| {
        get_view_ix(&compiler.schedule, &upstream[UPSTREAM_DOWNSTREAM])
    }).collect();
    Node{
        id: view_id.as_str().to_string(),
        view: view,
        upstream: upstream,
        downstream: downstream,
    }
}

fn create_flow(compiler: Compiler) -> Flow {
    let mut nodes = Vec::new();
    let mut dirty = BitSet::new();
    let mut states: Vec<Option<RefCell<Relation>>> = Vec::new();

    // compile nodes
    for (ix, schedule) in compiler.schedule.iter().enumerate() { // arrives in ix order
        let view_id = &schedule[SCHEDULE_VIEW];
        let view = compiler.flow.get_state("view").find_one(VIEW_ID, view_id).clone();
        let view_kind = &view[VIEW_KIND];
        let node = create_node(&compiler, view_id, view_kind);
        match node.view {
            View::Input => (),
            _ => {
                dirty.insert(ix);
            },
        }
        nodes.push(node);
        states.push(None);
    }

    // grab state from old flow
    let Compiler{flow, upstream, schedule, ..} = compiler;
    match nodes.iter().position(|node| &node.id[..] == "upstream") {
        Some(ix) => states[ix] = Some(RefCell::new(upstream)),
        None => (),
    }
    match nodes.iter().position(|node| &node.id[..] == "schedule") {
        Some(ix) => states[ix] = Some(RefCell::new(schedule)),
        None => (),
    }
    let Flow{nodes: old_nodes, states: old_states, changes, ..} = flow;
    for (old_node, old_state) in old_nodes.iter().zip(old_states.into_iter()) {
        if (old_node.id != "upstream") || (old_node.id != "schedule") {
            match nodes.iter().position(|node| node.id == old_node.id) {
                Some(ix) => states[ix] = Some(old_state),
                None => (),
            }
        }
    }

    // fill in state for new nodes
    let states = states.map_in_place(|state_option| match state_option {
        Some(state) => state,
        None => RefCell::new(Index::new()),
    });

    Flow{
        nodes: nodes,
        dirty: dirty,
        states: states,
        changes: changes,
    }
}
impl Flow {
    fn compiler_views_changed_since(&self, changes_seen: usize) -> bool {
        self.changes[changes_seen..].iter().any(|&(ref change_id, _)|
            COMPILER_VIEWS.iter().any(|view_id| *view_id == change_id)
            ));
        self.changes[changes_seen..].iter().any(|&(ref change_id, _)|
            COMPILER_VIEWS.iter().any(|view_id| *view_id == change_id)
            )
    }

    pub fn compile(mut self) -> Self {
        for view in COMPILER_VIEWS.iter() {
            self.ensure_input_exists(view);
        }
        let upstream = create_upstream(&self);
        let schedule = create_schedule(&self);
        let ordered_constraint = create_ordered_constraint(&self);
        let compiler = Compiler{
            flow: self,
            upstream: upstream,
            schedule: schedule,
            ordered_constraint: ordered_constraint,
        };
        create_flow(compiler)
    }

    pub fn compile_and_run(self) -> Self {
        let mut flow = self;
        let mut changes_seen = 0;
        if flow.compiler_views_changed_since(changes_seen) {
            changes_seen = flow.changes.len();
            flow = flow.compile();
        }
        loop {
            flow.run();
            if flow.compiler_views_changed_since(changes_seen) {
                changes_seen = flow.changes.len();
                flow = flow.compile();
            } else {
                return flow;
            }
        }
    }
}