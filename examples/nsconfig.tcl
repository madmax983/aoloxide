#
# Example aoloxide configuration, adapted from AOLserver's
# examples/config/base.tcl.  Run from the repository root:
#
#   cargo run --release -- --install-home examples/home
#   cargo run --release -- -f -t examples/nsconfig.tcl
#
# then browse http://127.0.0.1:8000/
#

set home [file join [file dirname [ns_info config]] home]
set pageRoot $home/servers/server1/pages

ns_section "ns/parameters"
    ns_param home $home
    ns_param logdebug false

ns_section "ns/mimetypes"
    ns_param default "*/*"
    ns_param .adp "text/html; charset=utf-8"

ns_section "ns/servers"
    ns_param server1 "server1"

ns_section "ns/server/server1"
    ns_param directoryfile "index.adp,index.html,index.htm"
    ns_param pageroot $pageRoot
    ns_param maxthreads 20
    ns_param minthreads 2
    ns_param urlcharset "utf-8"
    ns_param outputcharset "utf-8"
    ns_param inputcharset "utf-8"

ns_section "ns/server/server1/adp"
    ns_param map "/*.adp"

ns_section "ns/server/server1/modules"
    ns_param nssock nssock.so
    ns_param nslog nslog.so
    ns_param nsperm nsperm.so
    ns_param nscgi nscgi.so

ns_section "ns/server/server1/module/nssock"
    ns_param hostname 127.0.0.1
    ns_param address 127.0.0.1
    ns_param port 8000

ns_section "ns/server/server1/module/nslog"
    ns_param rolllog true
    ns_param rollonsignal true
    ns_param rollhour 0
    ns_param maxbackup 2

ns_section "ns/server/server1/module/nscgi"
    ns_param map "GET /cgi-bin $home/servers/server1/cgi-bin"
    ns_param map "POST /cgi-bin $home/servers/server1/cgi-bin"
