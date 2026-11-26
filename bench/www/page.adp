<% set name [ns_queryget name world] %><html><body><h1>Hello, <%= [ns_quotehtml $name] %></h1><ul><%
for {set i 1} {$i <= 10} {incr i} { ns_adp_puts -nonewline "<li>row $i</li>" }
%></ul></body></html>
