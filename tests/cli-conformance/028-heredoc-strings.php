<?php
$name = "World";
$h = <<<EOT
Hello $name
  indented {$name}s
EOT;
echo $h, "\n";
$n = <<<'EOT'
No $interpolation here \n
EOT;
echo $n, "\n";
echo "escapes: \x41 \101 \u{1F600} \t|\v|\f|\$literal \{not} end\n";
echo 'single \n \' quotes', "\n";
$arr = ["k" => ["n" => 5]];
echo "complex: {$arr['k']['n']}", "\n";
$flat = ["k" => 5];
echo "simple: $flat[k]", "\n";
$obj = new stdClass();
$obj->prop = "objprop";
echo "obj: $obj->prop / {$obj->prop}", "\n";
