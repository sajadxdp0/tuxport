# fork, pipes, waiting for child processes, signals, and the exit status of jobs
( for i in 1 2 3 4 5 6; do (echo "child $i") & done; wait ) | sort
sh -c 'exit 7'; echo "status $?"
sh -c 'kill -TERM $$'; echo "status $?"
trap 'echo caught USR1' USR1
kill -USR1 $$
echo after
echo piped | (read x; echo "got $x")
seq 1 7 | xargs -n 3 echo
i=0; while [ $i -lt 50 ]; do i=$((i + 1)); /bin/true; done; echo "$i forks"
