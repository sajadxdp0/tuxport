# Ruby: threads, fork, pipes, sockets, gem
ruby -e 'puts RUBY_VERSION; t = 4.times.map { |i| Thread.new { (1..500_000).sum { |x| x % 7 } } }; puts t.map(&:value).sum'
ruby -e 'r, w = IO.pipe; pid = fork { r.close; w.puts "from child #{Process.pid > 0}"; exit!(7) }; w.close; puts r.read; Process.wait(pid); puts $?.exitstatus'
ruby -rsocket -e 's = TCPServer.new("127.0.0.1", 0); port = s.addr[1]; th = Thread.new { c = s.accept; c.puts c.gets.upcase; c.close }; c = TCPSocket.new("127.0.0.1", port); c.puts "ping"; puts c.gets; th.join'
ruby -e 'require "json"; require "open3"; o, st = Open3.capture2("echo", "hi"); puts JSON.generate({o: o.strip, s: st.success?})'
gem list 2>/dev/null | head -3
