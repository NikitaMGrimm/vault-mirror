# Limitations

[← Documentation](README.md)

Tests cover Vaultwarden 1.37 and Bitwarden's self-hosted server 2026.9 as source and destination.
Testing also covers Bitwarden's cloud.

vault-mirror has these limitations:

| Limitation | Behavior |
| --- | --- |
| Sends and account settings | It does not copy these data. |
| Organization members, groups, and collection permissions | It does not copy these data. |
| Source trash | It does not copy items in the source trash. |
| Destination trash | It leaves these items unchanged. The server empties its own trash. |
| Bank accounts, driver's licences, and passports | Bitwarden shows these new item types only to enabled accounts. Bitwarden would hide copies of these types in a destination without access. vault-mirror excludes these types with a warning until Bitwarden enables them. |
| Archived items | The destination needs archive support. Otherwise, vault-mirror copies items without archive state and reports a warning. |
| Personal Bitwarden accounts without Premium | It copies other data and reports a warning for new attachments. It keeps attachments that it copied earlier. It copies new attachments after the account receives Premium. |
| Bitwarden organization items | Bitwarden's servers require a collection for each item. vault-mirror excludes source items without a collection from these destinations and reports a warning. |
| Attachment memory | It holds attachment content in memory during copying. A large attachment needs several times its size in free memory. |

Bitwarden accepts new attachments only from Premium accounts.

Bitwarden's cloud permits about 60 individual item creations per minute.
vault-mirror imports large groups of new personal items in bulk and waits after rate limits.
A large first run takes longer because the tool waits for rate limits to expire.
It creates new organization items one at a time.
