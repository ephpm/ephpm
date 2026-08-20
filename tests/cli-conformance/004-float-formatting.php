<?php
echo 0.1 + 0.2, "\n";
var_dump(0.1 + 0.2);
echo 1 / 3, "\n";
echo 1e100, "\n", -1.5e-10, "\n", 0.5, "\n", 100.0, "\n";
echo (string)(0.1 + 0.2) === '0.3' ? "eq" : "ne", "\n";
var_dump(1e15, 1e16, 1e17, 123456789.12345678);
printf("%.20f\n", 0.1);
echo json_encode([0.1 + 0.2, 1.0, 1e100]), "\n";
var_export(0.1 + 0.2);
echo "\n";
var_export(1.0);
echo "\n";
echo ini_get('precision'), ' ', ini_get('serialize_precision'), "\n";
echo 0.30000000000000004, "\n";
echo -0.0, " ", (string)-0.0, "\n";
