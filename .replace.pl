local $/;
open(F1, '<', '.old_hash.txt') or die;
my $old = <F1>;
open(F2, '<', '.new_hash.txt') or die;
my $new = <F2>;
open(F3, '<', 'lofi-core/src/compact.rs') or die;
my $content = <F3>;
my $idx = index($content, $old);
if ($idx == -1) { print "NOT FOUND
"; exit; }
substr($content, $idx, length($old)) = $new;
open(F4, '>', 'lofi-core/src/compact.rs') or die;
print F4 $content;
print "OK
";