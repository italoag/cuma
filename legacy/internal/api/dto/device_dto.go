package dto

type UpdateDeviceRequest struct {
	UserLabel string   `json:"user_label"`
	Tags      []string `json:"tags"`
}

type PaginationMeta struct {
	Total      int64 `json:"total"`
	Page       int   `json:"page"`
	PerPage    int   `json:"per_page"`
	TotalPages int   `json:"total_pages"`
}
