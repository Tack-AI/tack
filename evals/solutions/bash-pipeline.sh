awk '{print $2}' server.log | sort | uniq -c | awk '{print $2"="$1}' > stats.txt
