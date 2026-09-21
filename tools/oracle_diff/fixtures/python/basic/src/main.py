from shapes import Circle, Square


def helper():
    return 7


def total():
    c = Circle(2)
    s = Square(3)
    return c.area() + s.area()  # constructor-binding → Circle.area / Square.area


def run():
    helper()  # same-file bare call → main.helper
    total()   # same-file → main.total
