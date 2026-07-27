#!/bin/sh
p=/home/sirn/.local/state/lofi/sessions/home-sirn-Dev-src-git.sr.ht-~sirn-lofi/1784908657580_c5b95bfc2ac444df8ab071d0c81eaf4f.jsonl
for leaf in 3e543441cee84aff892bfa2c93813edd fa06408b278a4c2eb92b8042d955b8de e872f7eab89d4fa79b33b38186f0f90b d9f045952f5e481db5a2cc451718a9a4 dcc407b3301849b181617a3e372ae35a; do
 echo LEAF:$leaf
 awk -F '\t' -v leaf="$leaf" '{p[$1]=$2; n[$1]=NR; t[$1]=$3; r[$1]=$4} END {x=leaf; k=0; while(x!="" && n[x] && k<=NR){if(t[x]=="compaction" || (t[x]=="message"&&r[x]=="user")) out[++j]=n[x]+1; x=p[x]; k++} m=(j<15?j:15); for(i=m;i>=1;i--) print out[i]}' /tmp/lofi-graph.tsv > /tmp/line-nos
 while read line_no; do
  sed -n "${line_no}p" "$p" | jq -r '[.id,.type,.role,((([.blocks[]?|select(.type=="text")|.text]|first)//.summary//"")|gsub("\n";" ")|.[0:100])]|@tsv'
 done < /tmp/line-nos
done
