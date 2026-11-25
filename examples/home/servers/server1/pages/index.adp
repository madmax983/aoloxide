<html>
<head><title>aoloxide</title></head>
<body>
<h1>Hello from <%= [ns_info name] %> <%= [ns_info version] %></h1>
<p>Served by aoloxide (AOLserver ported to Rust) at <%= [ns_fmttime [ns_time]] %>.</p>
<%
  set form [ns_conn form]
  set name [ns_set get $form name]
  if {$name eq ""} { set name "world" }
%>
<p>Hello, <%= [ns_quotehtml $name] %>! (try <a href="?name=you">?name=you</a>)</p>
<p>This page has been viewed <%= [nsv_incr demo hits] %> times since startup.</p>
</body>
</html>
