# Benchmark endpoints (bench/README.md).

# GET /hello: the plaintext case, through a Tcl request proc.
proc bench_hello {} {
    ns_return 200 text/plain "Hello, World!"
}

ns_register_proc GET /hello bench_hello
