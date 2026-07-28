use strict; use warnings;
my $keep = qr{(?:# Errors|# Panics|# Safety|SAFETY|because|otherwise|instead of|rather than|cannot|can't|must not|must remain|invariant|compatib|workaround|quirk|race|security|soundness|unsound|deadlock|TOCTOU|path traversal|upstream|regression|no safe|not safe|to avoid|to prevent|so that|so (?:a|the|we)|without (?:allocat|los|exhaust|block|depend|holding|requiring)|would (?:otherwise|let|cause|risk)|only mechanism|only .* can|rejected)}i;
for my $file (@ARGV) {
  open my $in, '<', $file or die "$file: $!"; my @lines = <$in>; close $in; my @out;
  for (my $i = 0; $i < @lines;) {
    if ($lines[$i] =~ m{^\s*//}) { my @block; push @block, $lines[$i++] while $i < @lines && $lines[$i] =~ m{^\s*//}; my $text = join('', @block); push @out, @block if $text =~ $keep; }
    else { push @out, $lines[$i++]; }
  }
  open my $out, '>', $file or die "$file: $!"; print $out @out; close $out;
}
