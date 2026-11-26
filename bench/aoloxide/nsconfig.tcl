#
# aoloxide side of the axum comparison (see bench/README.md).
#
# Only nssock is loaded: no access log (axum logs nothing), no auth, no CGI.
# BENCH_HOME, BENCH_PORT, BENCH_THREADS and BENCH_PAGES come from bench/run.sh.
#

set home    $env(BENCH_HOME)
set port    [expr {[info exists env(BENCH_PORT)] ? $env(BENCH_PORT) : 8080}]
set threads [expr {[info exists env(BENCH_THREADS)] ? $env(BENCH_THREADS) : 20}]
set pages   $env(BENCH_PAGES)

ns_section "ns/parameters"
    ns_param home $home
    ns_param logdebug false

ns_section "ns/mimetypes"
    ns_param default "*/*"
    ns_param .adp "text/html; charset=utf-8"

ns_section "ns/servers"
    ns_param bench "bench"

ns_section "ns/server/bench"
    ns_param directoryfile "index.html"
    ns_param pageroot $pages
    ns_param minthreads $threads
    ns_param maxthreads $threads

ns_section "ns/server/bench/adp"
    ns_param map "/*.adp"

ns_section "ns/server/bench/modules"
    ns_param nssock nssock.so

ns_section "ns/server/bench/module/nssock"
    ns_param hostname 127.0.0.1
    ns_param address 127.0.0.1
    ns_param port $port
    # Default is 100; the benchmark runs up to 256 keep-alive connections.
    ns_param maxsock 1024
