//! Call sequencing (`--sequencing`; docs/ANALYSIS.md, "Call sequencing
//! (control-flow sidecar)").

use crate::common::{default_opts, fixture};

#[test]
fn sequencing_is_off_by_default() {
    let root = fixture("direct_call");
    let program = trace_parse::build_program(&root, &default_opts(&root)).expect("build");
    assert!(!program.call_sequencing);
    assert!(program.cfgs.is_empty());
}

#[test]
fn sequencing_flag_reaches_the_program() {
    let root = fixture("direct_call");
    let opts = default_opts(&root).with_call_sequencing(true);
    let program = trace_parse::build_program(&root, &opts).expect("build");
    assert!(program.call_sequencing);
}
