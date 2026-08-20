<?php
echo json_encode(["a" => 1, "b" => [true, null, 1.5], "c" => "x/y", "d" => "\u{FC}n\u{EF}"]), "\n";
echo json_encode(["x" => "a/b", "u" => "\u{E9}"], JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE), "\n";
echo json_encode(["n" => "123", "f" => "1.5"], JSON_NUMERIC_CHECK), "\n";
echo json_encode([1.0, 2.5], JSON_PRESERVE_ZERO_FRACTION), "\n";
echo json_encode(["k" => 1], JSON_PRETTY_PRINT), "\n";
echo json_encode([]), " ", json_encode(new stdClass()), " ", json_encode([1 => "a"]), "\n";
var_dump(json_decode('{"big": 123456789012345678901234567890}'));
var_dump(json_decode('{"big": 12345678901234567890123}', true, 512, JSON_BIGINT_AS_STRING));
var_dump(json_decode('9223372036854775808'));
var_dump(json_encode("\xB1\x31"));
echo json_last_error(), " ", json_last_error_msg(), "\n";
try {
    json_encode("\xB1\x31", JSON_THROW_ON_ERROR);
} catch (JsonException $e) {
    echo get_class($e), ": ", $e->getMessage(), "\n";
}
var_dump(json_decode('invalid'), json_last_error_msg());
var_dump(json_decode('null'), json_decode('"s"'), json_decode('1e2'));
echo json_encode(["inv" => "\xB1\x31"], JSON_INVALID_UTF8_SUBSTITUTE), "\n";
var_dump(json_validate('{"ok":1}'), json_validate('{nope'));
