# The Lua and SQLite command-line programs
lua5.4 -e '
local t = {}
for i = 1, 100000 do t[i] = i * i end
local s = 0
for _, v in ipairs(t) do s = s + v end
print(s, string.format("%.17g", math.pi), math.type(1 // 1), 7 // 2, 7.0 // 2, 2 ^ 53, ("x"):rep(3))
local words = {}
for w in ("the quick brown fox jumps over the lazy dog"):gmatch("%a+") do words[#words + 1] = w end
table.sort(words)
print(table.concat(words, ","))
'
sqlite3 :memory: "
create table t(x integer);
with recursive c(i) as (select 1 union all select i + 1 from c where i < 10000) insert into t select i from c;
select count(*), sum(x), avg(x * 1.0), total(x * 0.5) from t;
select group_concat(x % 10, '') from (select x from t order by x desc limit 30);
select printf('%.10f', exp(1)), hex(zeroblob(2)), upper('ß'), length('İ');
"
