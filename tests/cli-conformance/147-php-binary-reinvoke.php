<?php
// PHP_BINARY must be re-invocable AS php: spatie/async, Symfony Process
// (PhpExecutableFinder), Composer's `@php` handler and PHPUnit process
// isolation all run `PHP_BINARY <php-cli args>`. Under ePHPm, PHP_BINARY is the
// ephpm executable, which must then answer like php-cli for every argv shape
// below. (The bare, stdin-fed form with no arguments at all, which used to
// start `ephpm dev`, is covered by the cli e2e suite: every call here passes
// `-n` first.)
$run = function (array $args, string $stdin = ''): void {
    $p = proc_open(
        array_merge([PHP_BINARY, '-n'], $args),
        [0 => ['pipe', 'r'], 1 => ['pipe', 'w'], 2 => ['pipe', 'w']],
        $pipes
    );
    fwrite($pipes[0], $stdin);
    fclose($pipes[0]);
    $out = stream_get_contents($pipes[1]);
    $err = stream_get_contents($pipes[2]);
    fclose($pipes[1]);
    fclose($pipes[2]);
    echo json_encode(['out' => $out, 'err' => $err, 'code' => proc_close($p)]), "\n";
};

$run(['-r', 'echo "r:", 6 * 7;']);
$run(['-d', 'precision=3', '-r', 'echo M_PI;']);

$child = getenv('CONFORMANCE_TMPDIR') . '/child.php';
file_put_contents($child, '<?php echo "script:", $argv[1], ",", PHP_SAPI, ",", $argc;');
$run([$child, 'arg1']);

$run([], '<?php echo "stdin:", 1 + 1;');
$run(['--', 'x'], '<?php echo "argv:", implode("|", $argv);');
$run(['-r', 'exit(3);']);
