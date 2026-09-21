import { Circle } from './shapes';

function helper(): number { return 7; }

function total(): number {
    const c = new Circle(2);
    return c.describe() + helper();
}

function run(): number {
    return total();
}
