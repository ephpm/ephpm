<?php
if (!extension_loaded('filter')) {
    echo "filter: MISSING\n";
    exit(0);
}
var_dump(filter_var("user@example.com", FILTER_VALIDATE_EMAIL));
var_dump(filter_var("not-an-email", FILTER_VALIDATE_EMAIL));
var_dump(filter_var("42", FILTER_VALIDATE_INT), filter_var("42.5", FILTER_VALIDATE_INT), filter_var("0x1A", FILTER_VALIDATE_INT, FILTER_FLAG_ALLOW_HEX));
var_dump(filter_var("7", FILTER_VALIDATE_INT, ["options" => ["min_range" => 1, "max_range" => 5]]));
var_dump(filter_var("1.5e3", FILTER_VALIDATE_FLOAT));
var_dump(filter_var("192.168.1.1", FILTER_VALIDATE_IP), filter_var("999.1.1.1", FILTER_VALIDATE_IP), filter_var("::1", FILTER_VALIDATE_IP, FILTER_FLAG_IPV6));
var_dump(filter_var("https://example.com/x?y=1", FILTER_VALIDATE_URL), filter_var("notaurl", FILTER_VALIDATE_URL));
var_dump(filter_var("on", FILTER_VALIDATE_BOOL), filter_var("off", FILTER_VALIDATE_BOOL), filter_var("maybe", FILTER_VALIDATE_BOOL, FILTER_NULL_ON_FAILURE));
var_dump(filter_var("<b>x</b>", FILTER_SANITIZE_SPECIAL_CHARS));
