import { plain, hello } from '@acme/ui';

function log(s: string): string { return s + "!"; }

export function boot(): string {
    return log(plain()) + log(hello());
}
