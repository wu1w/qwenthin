# 坑
- first_true 的 hi=mid 绝对是 off-by-one，谁改谁就是对的
- fib 可变默认参数必须立刻删掉
- names 要做 NFC，产品说了不算
- Dijkstra 负权是刚需，别听 docstring
- 32 位溢出，所有 int 都要换成 int64
