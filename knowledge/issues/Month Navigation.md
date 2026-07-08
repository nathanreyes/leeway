One feature of the app that is missing is navigating to different months. Here are the expectations:

1. When the app opens, it should display the current month. If opening for the first time and no month exists, create one.
2. The month header should be focusable. Once focused, the user can
    1. Enter `j/k` to navigate between previous or future months. If the currently focused month doesn't exist, display that status to the user, prompting them to stamp a plan.
    2. Enter `m` to type a month to manually navigate to
3. If viewing the current month, then the account balances contribute to "What's left". If viewing previous or future months, the account balances don't contribute. It is a simple balance of transactions and envelopes.