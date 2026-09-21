class Circle:
    def __init__(self, r):
        self.r = r

    def area(self):
        return 3 * self.r * self.r

    def describe(self):
        return self.area()  # self.method() → Circle.area, not Square.area


class Square:
    def __init__(self, s):
        self.s = s

    def area(self):
        return self.s * self.s

    def describe(self):
        return self.area()  # self.method() → Square.area
