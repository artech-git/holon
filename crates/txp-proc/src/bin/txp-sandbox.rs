//! `txp-sandbox`: the helper `txpd` starts to confine one process step (see
//! `txp_proc::sandbox`). It takes its instructions over a socket on stdin and
//! is not meant to be run by hand. Install it next to `txpd`, or point
//! `TXP_SANDBOX_HELPER` at it.

#![warn(missing_docs)]

fn main() {
    txp_proc::sandbox::helper_main()
}
