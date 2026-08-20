<?php
var_dump(INF, -INF, NAN);
var_dump(is_nan(NAN), NAN == NAN, INF > PHP_FLOAT_MAX, is_finite(1.0), is_infinite(-INF));
echo INF, " ", -INF, " ", NAN, "\n";
var_dump(json_encode(INF));
echo json_last_error_msg(), "\n";
var_dump((int)INF, (int)-INF, (int)NAN);
var_dump(INF - INF, INF / INF, 0 * INF);
var_dump(NAN <=> NAN, NAN <=> 1.0, INF <=> PHP_FLOAT_MAX);
