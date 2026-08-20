<?php
echo $undefined_var;
echo "\n";
echo @$another_undefined;
echo "after-suppress\n";
$arr = [];
echo @$arr["missing"], "|\n";
echo $arr["missing2"] ?? "default", "\n";
trigger_error("custom notice", E_USER_NOTICE);
trigger_error("custom warning", E_USER_WARNING);
trigger_error("custom deprecated", E_USER_DEPRECATED);
var_dump(error_get_last()["message"]);
error_reporting(E_ALL & ~E_USER_WARNING);
trigger_error("hidden warning", E_USER_WARNING);
echo "still running\n";
error_reporting(E_ALL);
set_error_handler(function ($no, $str) {
    echo "handler: $str\n";
    return true;
});
trigger_error("handled", E_USER_WARNING);
restore_error_handler();
var_dump("12abc" + 1);
$u = [][0] ?? "nested-default";
echo $u, "\n";
echo strlen(null ?? ""), "\n";
