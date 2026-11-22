# From --test-args-file: the arguments for lein test, one per line, blank
# lines skipped, each given as it is.
args_file='@ARGS_FILE@'
if [[ ! -f $args_file ]]; then
  echo "::error::@SHOWN@ (from --test-args-file) is missing or not a file; it must be there when the tests run"
  exit 1
fi
if [[ $(tr -d '\000\r' < "$args_file" | wc -c) -ne $(wc -c < "$args_file") ]]; then
  echo "::error::@SHOWN@ (from --test-args-file) has a NUL or carriage return; give one argument per line, with LF line endings"
  exit 1
fi
mapfile -t lines < "$args_file"
args=()
for line in "${lines[@]}"; do
  [[ -z $line ]] || args+=("$line")
done
if [[ ${#args[@]} -eq 0 ]]; then
  echo "::error::@SHOWN@ (from --test-args-file) has no arguments; give one per line, or leave out --test-args-file to run lein test's default selection"
  exit 1
fi
