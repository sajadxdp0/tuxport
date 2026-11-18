# Java (OpenJDK 21): platform and virtual threads, implicit null checks and deoptimization after
# C2 compilation, stack overflow guard pages (all relying on SIGSEGV), GC and weak references,
# child processes, HTTP over NIO, IPv6 loopback, file mappings and file locks, crypto, the signal
# handler thread and shutdown hooks triggered by SIGTERM; ZGC's multi-mapped heap; jcmd's attach
# (SIGQUIT + AF_UNIX). java is started through the symlink /usr/bin/java and finds libjli.so
# through /proc/self/exe
set -e
mkdir -p /tmp/diff-java
cd /tmp/diff-java
cat > Main.java <<'JAVA'
import java.io.*;
import java.math.*;
import java.net.*;
import java.net.http.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;
import java.util.stream.*;
import java.util.zip.*;
import javax.crypto.*;
import javax.crypto.spec.*;
import com.sun.net.httpserver.*;

public class Main {
    sealed interface Shape permits Circle, Rect {}
    record Circle(double r) implements Shape {}
    record Rect(double w, double h) implements Shape {}

    static double area(Shape s) {
        return switch (s) {
            case Circle c -> Math.PI * c.r() * c.r();
            case Rect r -> r.w() * r.h();
        };
    }

    static int hash(Object o) { return o.hashCode(); }

    static int depth;
    static void recurse() { depth++; recurse(); }

    static long spin(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += i % 7;
        return s;
    }

    public static void main(String[] args) throws Exception {
        // Platform threads, synchronized, wait/notify
        long[] results = new long[8];
        Thread[] ts = new Thread[8];
        Object lock = new Object();
        int[] counter = {0};
        for (int i = 0; i < ts.length; i++) {
            int k = i;
            ts[i] = new Thread(() -> {
                results[k] = spin(3_000_000 + k);
                synchronized (lock) { counter[0]++; lock.notifyAll(); }
            });
            ts[i].start();
        }
        synchronized (lock) { while (counter[0] < ts.length) lock.wait(); }
        for (Thread t : ts) t.join();
        System.out.println("threads " + Arrays.toString(results));

        // Virtual threads: continuations move stacks between ForkJoinPool carrier threads
        AtomicInteger vt = new AtomicInteger();
        try (var ex = Executors.newVirtualThreadPerTaskExecutor()) {
            for (int i = 0; i < 10_000; i++) {
                ex.submit(() -> { Thread.sleep(1); return vt.incrementAndGet(); });
            }
        }
        System.out.println("virtual threads " + vt.get());

        System.out.println("parallel " + IntStream.range(0, 2_000_000).parallel().mapToLong(i -> i % 13).sum());

        // Let C2 compile area, then use a new shape and null to trigger deoptimization and the
        // implicit null check (SIGSEGV)
        double sum = 0;
        for (int i = 0; i < 3_000_000; i++) {
            sum += area(i % 3 == 0 ? new Circle(i % 10) : new Rect(i % 7, i % 5));
            sum += hash("x" + (i & 7));
        }
        System.out.printf("jit %.3f%n", sum);
        try { area(null); } catch (NullPointerException e) { System.out.println("npe " + e.getMessage()); }
        try { hash(null); } catch (NullPointerException e) { System.out.println("npe " + e.getMessage()); }
        int[] arr = new int[3];
        try { arr[spin(10) > 0 ? 5 : 0] = 1; } catch (ArrayIndexOutOfBoundsException e) { System.out.println("aioobe " + e.getMessage()); }
        try { System.out.println(1 / (int) (spin(1) - spin(1))); } catch (ArithmeticException e) { System.out.println("arith " + e.getMessage()); }
        // Stack overflow relies on the guard pages on the thread stack; catching it a second time
        // shows the yellow zone was re-enabled
        for (int i = 0; i < 2; i++) {
            depth = 0;
            try { recurse(); } catch (StackOverflowError e) { System.out.println("stack overflow caught " + (depth > 1000)); }
        }

        // GC: lots of short-lived objects; weak references are cleared after System.gc()
        List<byte[]> keep = new ArrayList<>();
        long allocated = 0;
        for (int i = 0; i < 200_000; i++) {
            byte[] b = new byte[1024 + i % 512];
            allocated += b.length;
            if (i % 1000 == 0) keep.add(b);
        }
        Object weakTarget = new Object();
        var weak = new java.lang.ref.WeakReference<>(weakTarget);
        weakTarget = null;
        System.gc();
        System.out.println("gc " + allocated + " kept " + keep.size() + " weak cleared " + (weak.get() == null));

        // Child process (posix_spawn + jspawnhelper)
        Process p = new ProcessBuilder("/bin/sh", "-c", "echo from $0; exit 3", "sh").redirectErrorStream(true).start();
        String out = new String(p.getInputStream().readAllBytes(), StandardCharsets.UTF_8).trim();
        System.out.println("process " + out + " exit " + p.waitFor());

        // HTTP over loopback: both the server and HttpClient use the NIO Selector (epoll)
        HttpServer server = HttpServer.create(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0), 0);
        server.createContext("/", h -> {
            byte[] body = ("served " + h.getRequestURI()).getBytes(StandardCharsets.UTF_8);
            h.sendResponseHeaders(200, body.length);
            try (OutputStream os = h.getResponseBody()) { os.write(body); }
        });
        server.setExecutor(Executors.newFixedThreadPool(2));
        server.start();
        HttpClient client = HttpClient.newHttpClient();
        var resp = client.send(HttpRequest.newBuilder(URI.create("http://127.0.0.1:" + server.getAddress().getPort() + "/path?q=1")).build(),
                HttpResponse.BodyHandlers.ofString());
        System.out.println("http " + resp.statusCode() + " " + resp.body());
        server.stop(0);
        ((ExecutorService) server.getExecutor()).shutdown();

        // IPv6 loopback: the JVM checks whether /proc/net/if_inet6 has addresses, and if not
        // turns IPv6 off entirely
        try (ServerSocket ss = new ServerSocket(0, 1, InetAddress.getByName("::1"));
             Socket cs = new Socket(InetAddress.getByName("::1"), ss.getLocalPort());
             Socket as = ss.accept()) {
            cs.getOutputStream().write(42);
            System.out.println("ipv6 " + as.getInetAddress().getHostAddress() + " " + as.getInputStream().read());
        } catch (IOException e) {
            System.out.println("ipv6 " + e);
        }

        // Files: memory mapping, file locks, traversal
        Path dir = Files.createTempDirectory("diff-java");
        Path f = dir.resolve("data.bin");
        try (FileChannel ch = FileChannel.open(f, StandardOpenOption.CREATE, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            MappedByteBuffer mb = ch.map(FileChannel.MapMode.READ_WRITE, 0, 1 << 16);
            for (int i = 0; i < mb.capacity(); i += 4) mb.putInt(i, i * 31);
            mb.force();
            try (FileLock fl = ch.lock()) { System.out.println("lock " + fl.isValid()); }
        }
        ByteBuffer bb = ByteBuffer.wrap(Files.readAllBytes(f));
        System.out.println("mmap " + bb.getInt(4000) + " " + Files.size(f));
        Files.writeString(dir.resolve("a.txt"), "hello");
        try (Stream<Path> s = Files.walk(dir)) { System.out.println("walk " + s.map(x -> dir.relativize(x).toString()).sorted().toList()); }

        // crypto, zip, CRC32 (all have instruction intrinsics on aarch64)
        MessageDigest md = MessageDigest.getInstance("SHA-256");
        System.out.println("sha256 " + HexFormat.of().formatHex(md.digest("tuxport".getBytes(StandardCharsets.UTF_8))));
        Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
        c.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(new byte[16], "AES"), new GCMParameterSpec(128, new byte[12]));
        System.out.println("aes " + HexFormat.of().formatHex(c.doFinal("tuxport".getBytes(StandardCharsets.UTF_8))));
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("EC");
        kpg.initialize(256);
        KeyPair kp = kpg.generateKeyPair();
        Signature sig = Signature.getInstance("SHA256withECDSA");
        sig.initSign(kp.getPrivate());
        sig.update(new byte[100]);
        byte[] signature = sig.sign();
        sig.initVerify(kp.getPublic());
        sig.update(new byte[100]);
        System.out.println("ecdsa " + sig.verify(signature));
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        try (var gz = new GZIPOutputStream(bos)) { gz.write("abc".repeat(10000).getBytes(StandardCharsets.UTF_8)); }
        byte[] unz = new GZIPInputStream(new ByteArrayInputStream(bos.toByteArray())).readAllBytes();
        CRC32 crc = new CRC32();
        crc.update(unz);
        System.out.println("zip " + bos.size() + " " + unz.length + " " + Long.toHexString(crc.getValue()));

        System.out.println(new BigInteger("3").pow(500).mod(new BigInteger("1000000007")) + " " + new BigDecimal("1").divide(new BigDecimal("7"), MathContext.DECIMAL128));
        System.out.println(Math.sqrt(2) + " " + Math.exp(1) + " " + Math.sin(1e10) + " " + StrictMath.log(10) + " " + 1e21 + " " + -0.0 + " " + Double.MIN_VALUE + " " + (float) Math.PI);

        // Signals: sun.misc.Signal handlers run in a separate thread; finally SIGTERM makes the
        // JVM run the shutdown hooks and exit
        CountDownLatch hup = new CountDownLatch(1);
        sun.misc.Signal.handle(new sun.misc.Signal("HUP"), s -> { System.out.println("signal " + s.getName()); hup.countDown(); });
        sun.misc.Signal.raise(new sun.misc.Signal("HUP"));
        hup.await();
        Runtime.getRuntime().addShutdownHook(new Thread(() -> System.out.println("shutdown hook")));
        System.out.flush();
        sun.misc.Signal.raise(new sun.misc.Signal("TERM"));
        Thread.sleep(60_000);
        System.out.println("not reached");
    }
}
JAVA
# sun.misc.Signal draws an "internal proprietary API" warning, which is not shown when compiling
# succeeds
javac -d . Main.java 2>javac.log || { cat javac.log; exit 1; }
set +e
java -cp . Main
echo "exit $?"
java -XX:+UseZGC -cp . Main | md5sum
cat > Sleeper.java <<'JAVA'
public class Sleeper {
    public static void main(String[] a) throws Exception {
        System.out.println("sleeper up");
        Thread.sleep(600_000);
    }
}
JAVA
javac -d . Sleeper.java
java -cp . Sleeper > sleeper.out &
pid=$!
# The child may not have created sleeper.out yet: grep -s does not report a missing file
while ! grep -qs 'sleeper up' sleeper.out; do sleep 0.1; done
jcmd $pid VM.version | tail -n +2
kill $pid
wait $pid
echo "sleeper exit $?"
