-- Opt-outs from product and marketing email. Account email (verification,
-- password reset, account deletion) never reads this table.
--
-- A row means "opted out"; resubscribing deletes it. Deleting the user removes
-- it. A destructive password reset keeps it, because changing a password isn't
-- a request to start receiving marketing again.
CREATE TABLE email_opt_outs (
    user_id UUID PRIMARY KEY REFERENCES users(uuid) ON DELETE CASCADE,
    source TEXT NOT NULL CHECK (source IN ('one_click', 'page', 'support')),
    opted_out_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP
);
