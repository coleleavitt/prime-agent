set -e
{ echo "id,status"; for i in $(seq 1 40); do
    if [ $((i % 5)) -eq 0 ]; then echo "$i,error"; else echo "$i,ok"; fi
  done; } > report.csv
