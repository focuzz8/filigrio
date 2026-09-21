export class Circle {
    r: number;
    constructor(r: number) { this.r = r; }
    area(): number { return 3 * this.r * this.r; }
    perimeter(): number { return 6 * this.r; }
    describe(): number { return this.area() + this.perimeter(); }
}

export class Square {
    s: number;
    constructor(s: number) { this.s = s; }
    area(): number { return this.s * this.s; }
    perimeter(): number { return 4 * this.s; }
    describe(): number { return this.area() + this.perimeter(); }
}
