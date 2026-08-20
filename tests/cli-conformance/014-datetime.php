<?php
date_default_timezone_set('UTC');
$d = new DateTime('2024-02-29 12:34:56');
echo $d->format('Y-m-d H:i:s D N j n t L U'), "\n";
echo $d->format(DateTime::ATOM), "\n";
$d2 = (clone $d)->modify('+1 year');
echo $d2->format('Y-m-d'), "\n";
$tz = new DateTimeZone('America/New_York');
$ny = new DateTime('2024-07-04 00:00:00', $tz);
echo $ny->format('Y-m-d H:i:s P T e'), "\n";
$ny->setTimezone(new DateTimeZone('UTC'));
echo $ny->format('Y-m-d H:i:s P'), "\n";
$i = (new DateTime('2024-01-31'))->diff(new DateTime('2024-03-01'));
echo $i->format('%R %y years %m months %d days, %a total days'), "\n";
echo (new DateTimeImmutable('2024-01-31'))->add(new DateInterval('P1M'))->format('Y-m-d'), "\n";
var_dump(strtotime('2024-01-15 00:00:00 UTC'));
echo date('Y-m-d H:i:s', 0), " ", date('c', 1234567890), "\n";
echo gmdate('D, d M Y H:i:s', 1700000000), " GMT\n";
var_dump(checkdate(2, 30, 2024), checkdate(2, 29, 2024), mktime(0, 0, 0, 1, 1, 2000));
var_dump(DateTime::createFromFormat('!d/m/Y', '29/02/2024')->format('Y-m-d H:i:s'));
try {
    new DateTime('not a date');
} catch (Exception $e) {
    echo get_class($e), ": ", $e->getMessage(), "\n";
}
echo date_default_timezone_get(), "\n";
