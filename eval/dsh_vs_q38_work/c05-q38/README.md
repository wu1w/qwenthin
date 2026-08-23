merge_intervals 文档写了调用方会先排序。生产调用方没排序。
已修复：merge_intervals 现在内部按起点排序，接受未排序输入。回归见 tests/test_merge.py::test_unsorted_regression。
