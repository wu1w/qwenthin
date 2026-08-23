merge_intervals 文档写了调用方会先排序。生产调用方没排序。
已修复：merge_intervals 现在内部先排序（needle/merge.py），回归测试见 tests/test_merge.py。
