# Debian (glibc): OpenJDK 21 compiling and running a multithreaded program
cd /tmp; rm -rf jt; mkdir jt; cd jt
cat > T.java <<'J'
import java.util.concurrent.*;
import java.util.stream.*;
public class T { public static void main(String[] a) throws Exception {
    ExecutorService e = Executors.newFixedThreadPool(4);
    long s = IntStream.range(0, 8).mapToObj(i -> e.submit(() -> LongStream.range(0, 2_000_000).map(x -> x % 7).sum())).mapToLong(f -> { try { return f.get(); } catch (Exception x) { throw new RuntimeException(x); } }).sum();
    e.shutdown();
    System.out.println("java " + System.getProperty("java.version") + " sum=" + s + " cpus=" + Runtime.getRuntime().availableProcessors() + " maxheap=" + Runtime.getRuntime().maxMemory() / (1 << 20) + "MiB");
} }
J
s=$(date +%s); javac T.java && java T; echo "java: $? ($(( $(date +%s) - s ))s)"
