import { greet } from '@acme/ui';

export function boot(): string {
    return greet();   // MUST resolve to @acme/ui greet, NOT @acme/admin's
}
