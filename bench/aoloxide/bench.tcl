# Benchmark endpoints (bench/README.md).

# GET /hello: the plaintext case, through a Tcl request proc.
proc bench_hello {} {
    ns_return 200 text/plain "Hello, World!"
}

ns_register_proc GET /hello bench_hello

# AOLserver's default limits queue at most 100 requests (maxwait) and answer
# the rest 503.  axum queues without bound, so lift the cap: at 256
# connections every response should be a real 200, not a cheap 503.
ns_limits set default -maxwait 1024
