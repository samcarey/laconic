/*
 * Twilio - Api
 *
 * This is the public Twilio REST API.
 *
 * The version of the OpenAPI document: 1.55.0
 * Contact: support@twilio.com
 * Generated by: https://openapi-generator.tech
 */




#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiPeriodV2010PeriodAccountPeriodQueuePeriodMember {
    /// The SID of the [Call](https://www.twilio.com/docs/voice/api/call-resource) the Member resource is associated with.
    #[serde(rename = "call_sid", default, with = "::serde_with::rust::double_option", skip_serializing_if = "Option::is_none")]
    pub call_sid: Option<Option<String>>,
    /// The date that the member was enqueued, given in RFC 2822 format.
    #[serde(rename = "date_enqueued", default, with = "::serde_with::rust::double_option", skip_serializing_if = "Option::is_none")]
    pub date_enqueued: Option<Option<String>>,
    /// This member's current position in the queue.
    #[serde(rename = "position", default, with = "::serde_with::rust::double_option", skip_serializing_if = "Option::is_none")]
    pub position: Option<Option<i32>>,
    /// The URI of the resource, relative to `https://api.twilio.com`.
    #[serde(rename = "uri", default, with = "::serde_with::rust::double_option", skip_serializing_if = "Option::is_none")]
    pub uri: Option<Option<String>>,
    /// The number of seconds the member has been in the queue.
    #[serde(rename = "wait_time", default, with = "::serde_with::rust::double_option", skip_serializing_if = "Option::is_none")]
    pub wait_time: Option<Option<i32>>,
    /// The SID of the Queue the member is in.
    #[serde(rename = "queue_sid", default, with = "::serde_with::rust::double_option", skip_serializing_if = "Option::is_none")]
    pub queue_sid: Option<Option<String>>,
}

impl ApiPeriodV2010PeriodAccountPeriodQueuePeriodMember {
    pub fn new() -> ApiPeriodV2010PeriodAccountPeriodQueuePeriodMember {
        ApiPeriodV2010PeriodAccountPeriodQueuePeriodMember {
            call_sid: None,
            date_enqueued: None,
            position: None,
            uri: None,
            wait_time: None,
            queue_sid: None,
        }
    }
}

