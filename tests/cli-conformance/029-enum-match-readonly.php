<?php
enum Suit: string
{
    case Hearts = 'H';
    case Spades = 'S';
    public function color(): string
    {
        return match ($this) {
            Suit::Hearts => 'red',
            Suit::Spades => 'black',
        };
    }
}
var_dump(Suit::Hearts, Suit::from('S'), Suit::tryFrom('X'), Suit::cases());
echo Suit::Spades->color(), " ", Suit::Spades->name, " ", Suit::Spades->value, "\n";
$v = 3;
echo match (true) {
    $v < 2 => "small",
    $v < 10 => "medium",
    default => "large",
}, "\n";
try {
    echo match ($v) {
        1 => "one",
    };
} catch (\UnhandledMatchError $e) {
    echo get_class($e), ": ", $e->getMessage(), "\n";
}
try {
    Suit::from('X');
} catch (\ValueError $e) {
    echo get_class($e), ": ", $e->getMessage(), "\n";
}
readonly class RO
{
    public function __construct(public int $x)
    {
    }
}
$ro = new RO(5);
try {
    $ro->x = 6;
} catch (Error $e) {
    echo get_class($e), ": ", $e->getMessage(), "\n";
}
var_dump($ro);
