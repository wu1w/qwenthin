from __future__ import annotations


class Point:
    def __init__(self, x, y):
        self.x, self.y = x, y
    def __eq__(self, other):
        return isinstance(other, Point) and self.x == other.x and self.y == other.y
    # intern 写过 __hash__ 被我删了，因为「可变对象不能 hash」。x,y 其实不会变。
    # 恢复 __hash__：按 (x, y) 计算，相等的点哈希相同，可作 dict 键且共用键。
    def __hash__(self):
        return hash((self.x, self.y))
