import { greet } from '@acme/ui';

export function boot(): string {
    return greet();   // MUST resolve to @acme/ui greet.ts, THROUGH the barrel
}
