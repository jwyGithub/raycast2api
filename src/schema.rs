diesel::table! {
    accounts (id) {
        id -> BigInt,
        email -> Text,
        tag -> Nullable<Text>,
        access_token -> Text,
        refresh_token -> Nullable<Text>,
        obtained_at -> Nullable<Text>,
        user_json -> Nullable<Text>,
        available -> Integer,
        unavailable_reason -> Nullable<Text>,
        has_pro_features -> Nullable<Integer>,
        tier -> Nullable<Text>,
        sub_status -> Nullable<Text>,
        trial_days_left -> Nullable<BigInt>,
        current_period_end -> Nullable<BigInt>,
        trial_requests_per_hour -> Nullable<Double>,
        remaining_credits -> Nullable<Double>,
        total_credits -> Nullable<Double>,
        remaining_usd -> Nullable<Double>,
        next_credits_at -> Nullable<Text>,
        info_fetched_at -> Nullable<Text>,
        cooldown_until -> Nullable<BigInt>,
        created_at -> Text,
        updated_at -> Text,
    }
}

diesel::table! {
    request_logs (id) {
        id -> BigInt,
        request_id -> Text,
        created_at -> BigInt,
        method -> Nullable<Text>,
        path -> Nullable<Text>,
        model -> Nullable<Text>,
        upstream_model -> Nullable<Text>,
        inbound_json -> Nullable<Text>,
        upstream_json -> Nullable<Text>,
        response_text -> Nullable<Text>,
    }
}

diesel::allow_tables_to_appear_in_same_query!(accounts, request_logs);
