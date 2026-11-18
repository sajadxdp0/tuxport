# .NET: dotnet new, build, run (JIT, GC, thread pool, Task, child processes), cgroup memory limit
# and usage
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1 DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1 DOTNET_GENERATE_ASPNET_CERTIFICATE=false HOME=/root
dotnet --info 2>&1 | grep -E 'Version:|RID|OS Name' | head -4
cd /tmp; rm -rf app; s=$(date +%s); dotnet new console -o app >/tmp/dotnet-new.log 2>&1; echo "new: $? ($(( $(date +%s) - s ))s)"; tail -3 /tmp/dotnet-new.log
cd app || exit 1
cat > Program.cs <<'CS'
using System.Diagnostics;
var sw = Stopwatch.StartNew();
var tasks = Enumerable.Range(0, 8).Select(i => Task.Run(() => { long s = 0; for (int j = 0; j < 20_000_000; j++) s += j % 7; return s; })).ToArray();
Task.WaitAll(tasks);
var list = new List<byte[]>(); for (int i = 0; i < 2000; i++) list.Add(new byte[100_000]); list.Clear(); GC.Collect(); GC.WaitForPendingFinalizers();
var p = Process.Start(new ProcessStartInfo("sh", "-c \"echo child\"") { RedirectStandardOutput = true })!; var child = p.StandardOutput.ReadToEnd().Trim(); p.WaitForExit();
File.WriteAllText("/tmp/dn.txt", "data");
var gc = GC.GetGCMemoryInfo();
Console.WriteLine($"sum={tasks.Sum(t => t.Result)} child={child} file={File.ReadAllText("/tmp/dn.txt")} cpus={Environment.ProcessorCount} gcs={GC.CollectionCount(0)} totalAvail={gc.TotalAvailableMemoryBytes >> 20}MiB load={gc.MemoryLoadBytes >> 20}MiB");
CS
s=$(date +%s); dotnet build -c Release -v q -nologo 2>&1 | grep -E 'error|Warn|Elapsed' | head -5; echo "build: $? ($(( $(date +%s) - s ))s)"
dotnet bin/Release/*/app.dll
DOTNET_GCHeapHardLimit=0x10000000 dotnet bin/Release/*/app.dll
echo "== memory load with 400 MB in another process (cgroup memory.current, minus inactive_file in memory.stat)"
python3 -c 'b = bytearray(400 << 20)
for i in range(0, len(b), 4096): b[i] = 1
import time; time.sleep(30)' &
sleep 4; dotnet bin/Release/*/app.dll; kill %1
