# ZGDA rust client and test harness

Generates workload tests for ZeroG DA.


## Compilation

install rust and cargo
cargo build
cargo run -- --help



## --- Quick test-----
cargo run -- zerog-da-disperse

Run 3MB dispersals on ZGDA. Each request to ZGDA is limited to a 512K chunk.
Requests are rate limited to 6 requests per second and 6 max outstanding requests per second.

## TODO:
The rate limits are hardcoded in the program, other params can be specified on the command line.
