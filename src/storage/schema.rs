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
    model_aliases (alias) {
        alias -> Text,
        model_id -> Text,
        created_at -> BigInt,
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
        effort -> Nullable<Text>,
        inbound_json -> Nullable<Text>,
        upstream_json -> Nullable<Text>,
        response_text -> Nullable<Text>,
    }
}

diesel::table! {
    statistics (id) {
        id -> Text,
        request_id -> Text,
        api_key -> Nullable<Text>,
        api_key_masked -> Nullable<Text>,
        requested_at -> BigInt,
        started_at -> BigInt,
        finished_at -> Nullable<BigInt>,
        request_ip -> Nullable<Text>,
        account_email -> Text,
        requested_model -> Text,
        upstream_provider -> Text,
        upstream_model -> Text,
        requested_effort -> Nullable<Text>,
        requested_thinking_json -> Nullable<Text>,
        upstream_effort -> Nullable<Text>,
        call_type -> Text,
        status -> Text,
        http_status -> Nullable<Integer>,
        error_message -> Nullable<Text>,
        usage_json -> Nullable<Text>,
    }
}

diesel::allow_tables_to_appear_in_same_query!(accounts, model_aliases, request_logs, statistics);
